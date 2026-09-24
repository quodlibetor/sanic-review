//! How much of the screen each pane gets, and the tab bar collapsed and
//! hidden panes shrink to.

use color_eyre::eyre::{Result, WrapErr};
use ratatui::layout::Rect;
use sanic_store::Store;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::Pane;

/// Where the layout is kept, in the store's `poll_state`.
const KEY: &str = "tui.layout";

/// A pane's border, top and bottom.
const BORDERS: u32 = 2;

/// The fewest rows a shown pane gets, border included, so a pane never
/// vanishes because the others are long.
const MIN_HEIGHT: u32 = BORDERS + 1;

/// How much room a pane takes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Size {
    /// As many rows as its content, when there's room.
    #[default]
    Fit,
    /// All of it; the other panes are tabs.
    Full,
    /// A tab.
    Collapsed,
}

/// Each pane's [`Size`]. At most one is [`Size::Full`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sizes {
    owed: Size,
    mine: Size,
    activity: Size,
    log: Size,
}

impl Sizes {
    pub(super) fn get(self, pane: Pane) -> Size {
        match pane {
            Pane::Owed => self.owed,
            Pane::Mine => self.mine,
            Pane::Activity => self.activity,
            Pane::Log => self.log,
        }
    }

    /// Sets `pane`'s size; a full-screen pane makes any other one fit.
    pub(super) fn set(&mut self, pane: Pane, size: Size) {
        if size == Size::Full {
            for other in Pane::ALL {
                if self.get(other) == Size::Full {
                    *self.slot(other) = Size::Fit;
                }
            }
        }
        *self.slot(pane) = size;
    }

    fn slot(&mut self, pane: Pane) -> &mut Size {
        match pane {
            Pane::Owed => &mut self.owed,
            Pane::Mine => &mut self.mine,
            Pane::Activity => &mut self.activity,
            Pane::Log => &mut self.log,
        }
    }

    /// `z`: fit, then full screen, then collapsed, then fit again.
    pub(super) fn cycle(&mut self, pane: Pane) {
        let next = match self.get(pane) {
            Size::Fit => Size::Full,
            Size::Full => Size::Collapsed,
            Size::Collapsed => Size::Fit,
        };
        self.set(pane, next);
    }

    pub(super) fn full(self) -> Option<Pane> {
        Pane::ALL.into_iter().find(|&p| self.get(p) == Size::Full)
    }

    /// Whether `pane` has an area rather than a tab.
    pub(super) fn shown(self, pane: Pane) -> bool {
        match self.full() {
            Some(full) => full == pane,
            None => self.get(pane) != Size::Collapsed,
        }
    }
}

/// The layout saved by a previous run, or every pane fitting its content.
pub(super) fn load(store: &Store) -> Sizes {
    let saved = store.poll_state(KEY).and_then(|saved| {
        saved
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .wrap_err("parsing the saved layout")
    });
    match saved {
        Ok(sizes) => sizes.unwrap_or_default(),
        Err(err) => {
            warn!("reading the terminal UI's layout failed: {err:?}");
            Sizes::default()
        }
    }
}

/// Saves `sizes` for the next run to [`load`].
pub(super) fn save(store: &Store, sizes: Sizes) -> Result<()> {
    let json = serde_json::to_string(&sizes)?;
    store
        .set_poll_state(KEY, &json)
        .wrap_err("saving the terminal UI's layout")
}

/// Where each pane goes in `area`: its own area if it's shown, else a tab
/// in the returned tab bar, one row at the bottom. `content` is each
/// pane's rows, by [`Pane::index`].
pub(super) fn arrange(
    area: Rect,
    sizes: Sizes,
    content: [usize; 4],
) -> ([Option<Rect>; 4], Option<Rect>) {
    let tabs = Pane::ALL.iter().any(|&p| !sizes.shown(p));
    let (body, bar) = if tabs && area.height > 0 {
        let body = Rect {
            height: area.height - 1,
            ..area
        };
        let bar = Rect {
            y: area.y + body.height,
            height: 1,
            ..area
        };
        (body, Some(bar))
    } else {
        (area, None)
    };
    let shown: Vec<Pane> = Pane::ALL.into_iter().filter(|&p| sizes.shown(p)).collect();
    let heights = if sizes.full().is_some() {
        vec![u32::from(body.height)]
    } else {
        let wants: Vec<_> = shown.iter().map(|&p| (p, content[p.index()])).collect();
        fit(u32::from(body.height), &wants)
    };
    let mut areas = [None; 4];
    let mut y = u32::from(body.y);
    let bottom = u32::from(body.y) + u32::from(body.height);
    for (pane, height) in shown.into_iter().zip(heights) {
        // Too short a screen for every pane's minimum cuts off the last.
        let height = height.min(bottom - y);
        if height == 0 {
            break;
        }
        areas[pane.index()] = Some(Rect {
            y: u16::try_from(y).unwrap_or(u16::MAX),
            height: u16::try_from(height).unwrap_or(u16::MAX),
            ..body
        });
        y += height;
    }
    (areas, bar)
}

/// Heights, border included, for `panes` stacked in `available` rows,
/// given each one's content rows. The PR lists get their content's height
/// first and activity and the log share what's left, none getting more
/// than its content; then the log, or activity without it, takes the rows
/// still spare. PR lists too long for the screen split it in proportion to
/// their content, leaving the others their minimum.
fn fit(available: u32, panes: &[(Pane, usize)]) -> Vec<u32> {
    // Capped so the sums below can't overflow.
    let want = |rows: usize| {
        u32::try_from(rows.max(1)).map_or(u32::from(u16::MAX), |r| r.min(u32::from(u16::MAX)))
            + BORDERS
    };
    let is_list = |pane: Pane| matches!(pane, Pane::Owed | Pane::Mine);
    let lists: Vec<u32> = panes
        .iter()
        .filter(|(p, _)| is_list(*p))
        .map(|&(_, rows)| want(rows))
        .collect();
    let streams: Vec<u32> = panes
        .iter()
        .filter(|(p, _)| !is_list(*p))
        .map(|&(_, rows)| want(rows))
        .collect();

    let list_room = available.saturating_sub(MIN_HEIGHT * len(&streams));
    let lists = if lists.iter().sum::<u32>() <= list_room {
        lists
    } else {
        proportional(list_room, &lists)
    };
    let rest = available.saturating_sub(lists.iter().sum());
    let mut streams = share(rest, &streams);
    // Streams come in pane order, so the last is the log if it's shown.
    let spare = rest.saturating_sub(streams.iter().sum());
    if let Some(last) = streams.last_mut() {
        *last += spare;
    }

    let (mut lists, mut streams) = (lists.into_iter(), streams.into_iter());
    panes
        .iter()
        .map(|&(p, _)| {
            let next = if is_list(p) {
                lists.next()
            } else {
                streams.next()
            };
            next.unwrap_or(MIN_HEIGHT)
        })
        .collect()
}

fn len(heights: &[u32]) -> u32 {
    u32::try_from(heights.len()).unwrap_or(u32::MAX)
}

/// Splits `room`, which is less than `wants` add up to, in proportion to
/// what each wants above the minimum.
fn proportional(room: u32, wants: &[u32]) -> Vec<u32> {
    let extra = u64::from(room.saturating_sub(MIN_HEIGHT * len(wants)));
    let above: Vec<u64> = wants.iter().map(|&w| u64::from(w - MIN_HEIGHT)).collect();
    let total = above.iter().sum::<u64>().max(1);
    let mut heights: Vec<u32> = above
        .iter()
        .map(|&a| MIN_HEIGHT + u32::try_from(extra * a / total).unwrap_or(0))
        .collect();
    // Rounding down leaves a few rows over; hand them out from the top.
    let given = heights.iter().sum::<u32>() - MIN_HEIGHT * len(wants);
    let mut left = u32::try_from(extra).unwrap_or(0).saturating_sub(given);
    for (height, &want) in heights.iter_mut().zip(wants) {
        let more = left.min(want.saturating_sub(*height));
        *height += more;
        left -= more;
    }
    heights
}

/// Splits `room` evenly, but none gets more than it wants, and what one
/// doesn't want goes to the others. Each gets at least the minimum.
fn share(room: u32, wants: &[u32]) -> Vec<u32> {
    let mut order: Vec<usize> = (0..wants.len()).collect();
    order.sort_by_key(|&i| wants[i]);
    let mut heights = vec![0; wants.len()];
    let mut left = room;
    for (done, &i) in order.iter().enumerate() {
        let others = u32::try_from(wants.len() - done).unwrap_or(1);
        let height = wants[i].min(left / others).max(MIN_HEIGHT);
        heights[i] = height;
        left = left.saturating_sub(height);
    }
    heights
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heights(available: u32, panes: &[(Pane, usize)]) -> Vec<u32> {
        fit(available, panes)
    }

    /// Every pane, with these content rows.
    fn all(owed: usize, mine: usize, activity: usize, log: usize) -> [(Pane, usize); 4] {
        [
            (Pane::Owed, owed),
            (Pane::Mine, mine),
            (Pane::Activity, activity),
            (Pane::Log, log),
        ]
    }

    #[test]
    fn lists_fit_their_content_and_the_log_takes_the_rest() {
        assert_eq!(heights(40, &all(5, 5, 4, 2)), [7, 7, 6, 20]);
        // An empty pane still has a row, for saying so.
        assert_eq!(heights(40, &all(0, 5, 0, 0)), [3, 7, 3, 27]);
        // Without the log, activity takes the rest; without either, it's
        // left blank.
        let no_log = [(Pane::Owed, 5), (Pane::Mine, 5), (Pane::Activity, 4)];
        assert_eq!(heights(40, &no_log), [7, 7, 26]);
        assert_eq!(heights(40, &[(Pane::Owed, 5), (Pane::Mine, 5)]), [7, 7]);
    }

    #[test]
    fn activity_and_the_log_share_what_the_lists_leave() {
        // 26 left: the short activity takes 6, the log the other 20.
        assert_eq!(heights(40, &all(5, 5, 4, 200)), [7, 7, 6, 20]);
        assert_eq!(heights(40, &all(5, 5, 200, 200)), [7, 7, 13, 13]);
    }

    #[test]
    fn long_lists_split_the_screen_by_content_and_scroll() {
        // 34 rows for the lists after the others' minimums, split about
        // 3:1 above their own minimums.
        assert_eq!(heights(40, &all(90, 30, 200, 200)), [25, 9, 3, 3]);
        let [owed, mine, ..] = heights(40, &all(60, 60, 0, 0))[..] else {
            panic!()
        };
        assert_eq!((owed, mine), (17, 17));
        // Without the others, the lists have it all.
        assert_eq!(heights(40, &[(Pane::Owed, 60), (Pane::Mine, 20)]), [29, 11]);
    }

    #[test]
    fn a_tiny_screen_keeps_minimums() {
        assert_eq!(heights(10, &all(50, 50, 50, 50)), [3, 3, 3, 3]);
    }

    #[test]
    fn z_cycles_and_one_pane_at_most_is_full() {
        let mut sizes = Sizes::default();
        sizes.cycle(Pane::Mine);
        assert_eq!(sizes.full(), Some(Pane::Mine));
        assert!(!sizes.shown(Pane::Owed));
        sizes.cycle(Pane::Log);
        assert_eq!(sizes.full(), Some(Pane::Log));
        assert_eq!(sizes.get(Pane::Mine), Size::Fit);
        sizes.cycle(Pane::Log);
        assert_eq!(sizes.get(Pane::Log), Size::Collapsed);
        assert!(sizes.shown(Pane::Owed) && !sizes.shown(Pane::Log));
        sizes.cycle(Pane::Log);
        assert_eq!(sizes, Sizes::default());
    }

    #[test]
    fn arrange_leaves_a_tab_bar_for_hidden_panes() {
        let area = Rect::new(0, 0, 80, 30);
        let content = [5, 2, 4, 2];
        let (areas, bar) = arrange(area, Sizes::default(), content);
        assert_eq!(bar, None);
        assert_eq!(
            areas.map(|a| a.map(|a| (a.y, a.height))),
            [Some((0, 7)), Some((7, 4)), Some((11, 6)), Some((17, 13))]
        );

        let mut sizes = Sizes::default();
        sizes.set(Pane::Activity, Size::Full);
        let (areas, bar) = arrange(area, sizes, content);
        assert_eq!(bar, Some(Rect::new(0, 29, 80, 1)));
        assert_eq!(areas, [None, None, Some(Rect::new(0, 0, 80, 29)), None]);
    }

    #[test]
    fn the_layout_round_trips_through_the_store() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(load(&store), Sizes::default());
        let mut sizes = Sizes::default();
        sizes.set(Pane::Log, Size::Collapsed);
        sizes.set(Pane::Mine, Size::Full);
        save(&store, sizes).unwrap();
        assert_eq!(load(&store), sizes);
        // Something unreadable is the default, not an error.
        store.set_poll_state(KEY, "nonsense").unwrap();
        assert_eq!(load(&store), Sizes::default());
    }
}
