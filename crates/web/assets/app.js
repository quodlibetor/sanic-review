// Keyboard shortcuts for the dashboard: the TUI's keys, where it has them.
// No framework; the page works without this file, just without keys.
(function () {
  "use strict";

  // Pages style themselves for the script being here: a draft's body shows
  // as text to click, not as a box to type in.
  document.documentElement.classList.add("js");

  const page = document.body.dataset.page;
  const help = document.getElementById("help");
  const notice = document.getElementById("notice");
  const RERUN_HINT = "r reviews a failed, held, skipped or archived PR you owe now";

  // Lists you move in: the index's two lists, or a PR page's drafts.
  let focus = 0;
  // The selected row in each list, by what it's about (a PR's key or a
  // draft's id), so a refresh that reorders the rows keeps it; none until
  // you move.
  const selected = [];

  function lists() {
    if (page === "index") return Array.from(document.querySelectorAll(".list"));
    if (page === "queue") {
      const queue = document.querySelector("#queue ol.queue");
      return queue ? [queue] : [];
    }
    const drafts = document.getElementById("drafts");
    return drafts ? [drafts] : [];
  }

  // The rows you can move to: a folded group's are skipped.
  function rows(list) {
    const rowsOf = page === "index" || page === "queue" ? "[data-row]" : ".draft";
    const all = list.querySelectorAll(rowsOf);
    return Array.from(all).filter(function (row) {
      // The other tab's rows are hidden, but still its list to come back to.
      if (page === "index") return !row.closest("details:not([open])");
      return row.offsetParent !== null;
    });
  }

  function idOf(row) {
    return row.dataset.key || row.id;
  }

  // Where list i's selected row is among its rows, or -1.
  function indexIn(all, i) {
    if (selected[i] === undefined) return -1;
    return all.findIndex(function (row) {
      return idOf(row) === selected[i];
    });
  }

  function position(list, i) {
    return indexIn(rows(list), i);
  }

  function currentRow() {
    const list = lists()[focus];
    if (!list) return null;
    return rows(list)[position(list, focus)] || null;
  }

  function draw(scroll) {
    lists().forEach(function (list, i) {
      list.classList.toggle("focused", i === focus && page === "index");
      const all = rows(list);
      const at = indexIn(all, i);
      // A row that's gone, or folded away, leaves nothing selected.
      if (at < 0) selected[i] = undefined;
      list.querySelectorAll(".selected").forEach(function (row) {
        row.classList.remove("selected");
      });
      if (i === focus && at >= 0) all[at].classList.add("selected");
    });
    const row = currentRow();
    if (row && scroll) row.scrollIntoView({ block: "nearest" });
  }

  function moveTo(target) {
    const list = lists()[focus];
    if (!list) return;
    const all = rows(list);
    if (all.length === 0) return;
    selected[focus] = idOf(all[Math.max(0, Math.min(target, all.length - 1))]);
    // Moving on leaves a Copy that c focused, so Enter opens the draft
    // rather than copying again.
    const active = document.activeElement;
    if (active && active.matches("button[data-copy], .rb, .pp")) active.blur();
    draw(true);
  }

  function moveBy(delta) {
    const list = lists()[focus];
    if (!list) return;
    const at = position(list, focus);
    moveTo(at < 0 ? 0 : at + delta);
  }

  function say(text) {
    notice.textContent = text;
    notice.hidden = false;
    clearTimeout(say.timer);
    say.timer = setTimeout(function () {
      notice.hidden = true;
    }, 4000);
  }

  function go(href) {
    if (href) window.location.href = href;
  }

  function typing(target) {
    if (target.tagName === "INPUT") return !/^(checkbox|radio|button|submit)$/.test(target.type);
    return target.isContentEditable || /^(TEXTAREA|SELECT)$/.test(target.tagName);
  }

  // What r, x, c and i act on: the selected row on the index, the PR itself
  // on a PR page.
  function subject() {
    return page === "index" ? currentRow() : document.getElementById("pr-actions");
  }

  // A key held down, or typed just as the page opened or came to the
  // front, does nothing, so typing ahead ("yyyy") can't confirm. The server
  // keeps other sites from opening pages here; this catches keys meant for
  // whatever was in front before.
  const SETTLE_MS = 500;
  let shown = document.visibilityState === "visible" ? Date.now() : Infinity;
  document.addEventListener("visibilitychange", function () {
    shown = document.visibilityState === "visible" ? Date.now() : Infinity;
  });
  window.addEventListener("focus", function () {
    shown = Date.now();
  });

  // A confirm page's card, opened over the page you asked from: only by
  // your own click or key here, since it's this page's script that fetches
  // it. Keys settle again when it opens, and its form posts as the page's
  // would. Without the script, links go to the confirm page itself.
  const dialog = document.getElementById("dialog");
  // Only the latest ask opens: a slow answer to an earlier r or click
  // mustn't open over, or instead of, the one you asked for last.
  let asked = 0;
  // Where focus was before, to go back to.
  let opener = null;

  function dialogOpen() {
    return dialog && !dialog.hidden;
  }

  // While it's open the page behind takes no focus, so Tab can't reach its
  // buttons and Enter can't press them.
  function behind(inert) {
    Array.from(document.body.children).forEach(function (el) {
      if (el !== dialog && el !== notice) el.inert = inert;
    });
  }

  function openDialog(href) {
    const ask = ++asked;
    const from = document.activeElement;
    fetch(href, { credentials: "same-origin" })
      .then(function (resp) {
        if (!resp.ok) throw new Error(String(resp.status));
        return resp.text();
      })
      .then(function (text) {
        if (ask !== asked || dialogOpen()) return;
        const card = new DOMParser().parseFromString(text, "text/html").querySelector(".cf");
        if (!card) {
          go(href);
          return;
        }
        // Confirmed from the index, it comes back to the index, filtered as
        // it is.
        const next = card.querySelector('input[name="next"]');
        if (next && page === "index") {
          next.value = "index";
          const back = document.createElement("input");
          back.type = "hidden";
          back.name = "back";
          back.value = window.location.pathname + window.location.search;
          next.after(back);
        }
        const popup = dialog.firstElementChild;
        popup.replaceChildren(document.importNode(card, true));
        dialog.hidden = false;
        behind(true);
        opener = from;
        shown = Date.now();
        sendOnce(dialog.querySelector("#confirm"));
        // Focus is on the card, not a button, so a stray Enter or Space
        // presses nothing: y confirms, or Tab to a button. A card that asks
        // for text starts in its text box; y there is only a letter.
        popup.tabIndex = -1;
        const field = popup.querySelector("[autofocus]");
        (field || popup).focus();
      })
      .catch(function (err) {
        if (ask === asked) say("That failed (" + err.message + "); reload the page to see why.");
      });
  }

  function closeDialog() {
    dialog.hidden = true;
    dialog.firstElementChild.replaceChildren();
    behind(false);
    if (opener && opener.isConnected) opener.focus();
    opener = null;
  }

  function onKey(e) {
    // Keys typed into a box act on nothing, so they needn't settle: the
    // Agent card's box, focused as it opens, keeps its first letters.
    if (typing(e.target)) {
      // Esc leaves a draft's text box, which saves it, and the filter's,
      // which a search box would otherwise empty.
      if (e.key === "Escape" && !e.repeat) {
        if (e.target.type === "search") e.preventDefault();
        e.target.blur();
      }
      return;
    }
    // The filter's boxes take their own keys, Tab and Space among them;
    // Esc goes back to the lists.
    if (e.target.closest && e.target.closest("#filter")) {
      if (e.key === "Escape") {
        e.target.blur();
        e.preventDefault();
      }
      return;
    }
    // Not even the browser's own handling, like Space ticking a box.
    if (e.repeat || Date.now() - shown < SETTLE_MS) {
      e.preventDefault();
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (!help.hidden) {
      if (e.key === "?" || e.key === "Escape" || e.key === "q") {
        help.hidden = true;
        e.preventDefault();
      }
      return;
    }
    // An open dialog takes every key but the ones that move between and
    // press its buttons.
    if (dialogOpen()) {
      const confirm = dialog.querySelector("#confirm");
      if (e.key === "y" && confirm) confirm.requestSubmit();
      else if (e.key === "Escape" || e.key === "q" || e.key === "n") closeDialog();
      else if (e.key === "Tab") return;
      else if (e.key === "Enter" || e.key === " ") {
        // They press only the button you've moved to.
        if (e.target.closest("#dialog a, #dialog button")) return;
      }
      e.preventDefault();
      return;
    }
    if (page === "confirm") {
      const confirm = document.getElementById("confirm");
      const cancel = document.getElementById("cancel");
      if (e.key === "y" && confirm) confirm.requestSubmit();
      else if (e.key === "Escape" || e.key === "q" || e.key === "n") go(cancel && cancel.href);
      else if (e.key === "?") help.hidden = false;
      else return;
      e.preventDefault();
      return;
    }
    // Esc closes a popover that v, d or Tab opened.
    if (e.key === "Escape" && popover(document.activeElement)) {
      document.activeElement.blur();
      e.preventDefault();
      return;
    }
    switch (e.key) {
      case "?":
        help.hidden = false;
        break;
      case "Escape": {
        // A result card's way back.
        const back = document.querySelector("main .cf #cancel");
        if (!back) return;
        go(back.href);
        break;
      }
      case "q":
        if (page === "index") say("q goes back from other pages; close the tab to leave");
        else go("/");
        break;
      case "Tab": {
        const n = lists().length;
        if (page !== "index" || n === 0) return;
        if (popover(document.activeElement)) document.activeElement.blur();
        setTab((focus + (e.shiftKey ? n - 1 : 1)) % n);
        break;
      }
      case "j":
      case "ArrowDown":
        moveBy(1);
        break;
      case "k":
      case "ArrowUp":
        moveBy(-1);
        break;
      case "g":
      case "Home":
        moveTo(0);
        break;
      case "G":
      case "End":
        moveTo(Infinity);
        break;
      case "Enter": {
        const row = currentRow();
        if (!row || e.target !== document.body) return;
        if (page === "index" || page === "queue") go(row.dataset.href);
        else editDraft(row);
        break;
      }
      case "e": {
        const row = page === "pr" && currentRow();
        if (!row || !editDraft(row)) return;
        break;
      }
      case "y":
      case "n":
      case "u": {
        // Accept, reject or undo the selected draft, as its buttons do.
        const row = page === "pr" && currentRow();
        const status = { y: "accepted", n: "rejected", u: "pending" }[e.key];
        const button = row && row.querySelector('button[name="status"][value="' + status + '"]');
        if (!button) return;
        button.click();
        break;
      }
      case "a": {
        // Opens the selected draft's Revise box, ready for your note.
        const row = page === "pr" && currentRow();
        const revise = row && row.querySelector("details.revise");
        if (!revise) {
          if (page === "pr") say("a revises the selected draft with the agent, when its run has a session");
          return;
        }
        revise.open = true;
        revise.querySelector("textarea").focus();
        break;
      }
      case "p": {
        const preview = page === "pr" && document.getElementById("preview");
        if (!preview) return;
        preview.click();
        break;
      }
      case "r": {
        const target = subject();
        const href = target && target.dataset.reviewNow;
        if (href) openDialog(href);
        else say(RERUN_HINT);
        break;
      }
      case "x": {
        const target = subject();
        const form = target && target.querySelector("form.archive");
        if (form) form.requestSubmit();
        else say("x archives the selected PR");
        break;
      }
      case "Q":
        go("/queue");
        break;
      case "K":
      case "J": {
        // Move the selected queued run itself, rather than the cursor.
        const row = page === "queue" && currentRow();
        if (!row) return;
        const dir = e.key === "K" ? "up" : "down";
        const form = row.querySelector('form.qmove[data-dir="' + dir + '"]');
        const button = form && form.querySelector("button");
        if (!button || button.disabled) {
          // Disabled at the ends too, so say which it is.
          if (!row.hasAttribute("data-movable")) {
            say(
              row.classList.contains("running")
                ? "a running review can't be moved"
                : "a regeneration isn't in the queue"
            );
          } else {
            say(dir === "up" ? "already first in the queue" : "already last in the queue");
          }
          return;
        }
        form.requestSubmit();
        break;
      }
      case "X": {
        const panes = document.getElementById("panes");
        if (!panes) return;
        go(panes.dataset.archivedHref);
        break;
      }
      case "/": {
        const box = document.getElementById("fq");
        if (!box) return;
        document.getElementById("filter-fold").open = true;
        box.focus();
        box.select();
        break;
      }
      case "c": {
        if (page === "queue") {
          const row = currentRow();
          if (!row) return;
          openDialog(row.dataset.cancel);
          break;
        }
        const target = subject();
        const href = target && target.dataset.chat;
        // On a PR page it names the card there, "#chat".
        const here = href && href.charAt(0) === "#" && document.querySelector(href);
        if (here) {
          // Scroll to the card and ready its Copy.
          here.scrollIntoView({ block: "nearest" });
          const copy = here.querySelector("button[data-copy]");
          if (copy) copy.focus();
        } else if (href) go(href);
        else say("c chats with the agent that reviewed the selected PR");
        break;
      }
      case "v":
      case "d": {
        // The selected row's reviewers, or its lead's run and drafts: focus
        // opens the popover, and Esc closes it.
        const row = page === "index" && currentRow();
        const opener = row && row.querySelector(e.key === "v" ? ".rb" : ".pp");
        if (opener) opener.focus();
        else if (page === "index") say(e.key + " shows the selected row's " + (e.key === "v" ? "reviewers" : "run and drafts"));
        else return;
        break;
      }
      case "f": {
        // The PR page's other view of its drafts.
        const other = page === "pr" && document.querySelector("#views a[data-view]:not(.cur)");
        if (!other) return;
        other.click();
        break;
      }
      case "s": {
        // The files view's other layout.
        const other = page === "pr" && document.querySelector("#layouts a[data-layout]:not(.cur)");
        if (!other) return;
        other.click();
        break;
      }
      case "i": {
        const target = subject();
        const href = target && target.dataset.ignore;
        if (href) go(href);
        else say("i skips reviews you owe by title");
        break;
      }
      default:
        return;
    }
    e.preventDefault();
  }

  // A draft's body turns into its box, which saves as you leave it. The box
  // starts as tall as the text it replaces, and grows to fit what you type.
  function editDraft(card) {
    const box = card.querySelector(".edit textarea");
    if (!box) return false;
    const body = card.querySelector(".body");
    box.dataset.minHeight = body ? body.offsetHeight : 0;
    card.classList.add("editing");
    fit(box);
    box.focus();
    return true;
  }

  // Sizes a text box to its content, however long, so it never scrolls,
  // but never shorter than it first was: for a draft's box, the text it
  // replaced.
  function fit(box) {
    if (box.dataset.minHeight === undefined) box.dataset.minHeight = box.offsetHeight;
    // Collapsing it to measure can shorten the page, which would scroll it.
    const scroll = window.scrollY;
    box.style.height = "0";
    const border = box.offsetHeight - box.clientHeight;
    box.style.height = Math.max(box.scrollHeight + border, Number(box.dataset.minHeight)) + "px";
    if (window.scrollY !== scroll) window.scrollTo(window.scrollX, scroll);
  }
  document.addEventListener("input", function (e) {
    if (e.target.matches(".dc textarea, .cf textarea")) fit(e.target);
  });

  document.addEventListener("click", function (e) {
    const body = e.target.closest(".dc .body[data-edit]");
    // Links, folds and images in its Markdown are for reading it.
    if (body && !e.target.closest(".md a, .md summary, .md .md-img, .md img")) editDraft(body.closest(".dc"));
  });

  // An image in Markdown loads only when you ask: its placeholder has
  // where it's from, and becomes the image on a click, or Enter.
  function loadImage(placeholder) {
    const img = document.createElement("img");
    img.referrerPolicy = "no-referrer";
    img.alt = placeholder.dataset.alt || "";
    img.src = placeholder.dataset.src;
    placeholder.replaceWith(img);
  }
  document.addEventListener("click", function (e) {
    const placeholder = e.target.closest(".md .md-img[data-src]");
    if (!placeholder) return;
    // Not the link it may be in, this time.
    e.preventDefault();
    loadImage(placeholder);
  });
  document.addEventListener("keydown", function (e) {
    if (e.key !== "Enter" || !e.target.matches(".md .md-img[data-src]")) return;
    e.preventDefault();
    loadImage(e.target);
  });
  document.addEventListener("focusout", function (e) {
    const box = e.target.matches(".edit textarea") && e.target;
    if (!box) return;
    whenReleased(function () {
      if (document.activeElement !== box) box.closest(".dc").classList.remove("editing");
    });
  });

  // A click goes where the pointer went down and came up; if the drafts
  // move in between, as when the box you leave folds or its save swaps in,
  // it lands on neither. So while a pointer is down those wait, until just
  // after its click. Leaving by keyboard folds at once.
  let pressed = false;
  let held = [];
  // Saved drafts waiting to swap in, by the card they replace.
  const heldSwaps = new Map();
  function whenReleased(f) {
    if (pressed) held.push(f);
    else f();
  }
  function release() {
    if (!pressed) return;
    pressed = false;
    // After the click, which comes right after the pointer's up.
    setTimeout(function () {
      const run = held;
      held = [];
      run.forEach(function (f) {
        f();
      });
      heldSwaps.forEach(function (swap) {
        swap();
      });
      heldSwaps.clear();
    });
  }
  document.addEventListener(
    "pointerdown",
    function () {
      pressed = true;
    },
    true
  );
  document.addEventListener("pointerup", release, true);
  document.addEventListener("pointercancel", release, true);
  window.addEventListener("blur", release);
  document.body.addEventListener("htmx:beforeSwap", function (e) {
    const target = e.detail.target;
    if (!pressed || !target.matches(".dc") || !e.detail.shouldSwap) return;
    e.detail.shouldSwap = false;
    const content = e.detail.serverResponse;
    heldSwaps.set(target, function () {
      if (target.isConnected) htmx.swap(target, content, { swapStyle: "outerHTML" });
    });
  });
  // A newer request for the card answers with it as it is then, so a
  // save still waiting would only swap in something older.
  document.body.addEventListener("htmx:beforeRequest", function (e) {
    heldSwaps.delete(e.detail.target);
  });

  // Run times in your own time zone: "today 10:42", else the date.
  function localTimes(root) {
    const now = new Date();
    const dayOf = function (d) {
      return d.toDateString();
    };
    const yesterday = new Date(now.getTime() - 86400000);
    root.querySelectorAll("time[datetime]:not(.ago)").forEach(function (t) {
      const at = new Date(t.dateTime);
      if (isNaN(at.getTime())) return;
      const clock = at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
      const day =
        dayOf(at) === dayOf(now)
          ? "today"
          : dayOf(at) === dayOf(yesterday)
            ? "yesterday"
            : at.toLocaleDateString();
      t.title = at.toString();
      t.textContent = day + " " + clock;
    });
  }
  localTimes(document);

  // The index's popovers: who reviewed a PR (.rb), and what's behind its
  // lead (.pp). CSS shows them on hover and focus; while one is shown it's
  // pinned to the window beside its opener, so a row can't clip it.
  function popover(el) {
    return el && el.matches && el.matches(".rb, .pp") ? el : null;
  }
  function place(opener) {
    const pop = opener.querySelector(".pop");
    if (!pop) return;
    pop.classList.add("fixed");
    const r = opener.getBoundingClientRect();
    const w = pop.offsetWidth;
    const h = pop.offsetHeight;
    const left = Math.max(8, Math.min(r.left, window.innerWidth - w - 8));
    let top = r.bottom + 4;
    if (top + h > window.innerHeight - 8) top = Math.max(8, r.top - h - 4);
    pop.style.left = left + "px";
    pop.style.top = top + "px";
  }
  function unplace(opener) {
    const pop = opener.querySelector(".pop");
    if (!pop) return;
    pop.classList.remove("fixed");
    pop.style.left = "";
    pop.style.top = "";
  }
  document.addEventListener("mouseover", function (e) {
    const opener = e.target.closest && e.target.closest(".rb, .pp");
    if (opener && !opener.contains(e.relatedTarget)) place(opener);
  });
  document.addEventListener("mouseout", function (e) {
    const opener = e.target.closest && e.target.closest(".rb, .pp");
    if (opener && !opener.contains(e.relatedTarget) && document.activeElement !== opener) {
      unplace(opener);
    }
  });
  document.addEventListener("focusin", function (e) {
    if (popover(e.target)) place(e.target);
  });
  document.addEventListener("focusout", function (e) {
    if (popover(e.target) && !e.target.matches(":hover")) unplace(e.target);
  });
  window.addEventListener(
    "scroll",
    function () {
      const active = popover(document.activeElement);
      if (active) place(active);
      document.querySelectorAll(".rb:hover, .pp:hover").forEach(place);
    },
    { passive: true }
  );

  document.addEventListener("keydown", onKey);
  // A link to a confirm page opens its card over this page instead; a
  // click with a modifier, or a middle click, still opens the page.
  document.addEventListener("click", function (e) {
    const link = e.target.closest("a[data-dialog]");
    if (!link || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    openDialog(link.href);
  });
  if (dialog) {
    dialog.addEventListener("click", function (e) {
      // Clicks settle as keys do: the second click of a double click on a
      // link that opened it mustn't land on its Confirm.
      if (Date.now() - shown < SETTLE_MS) {
        e.preventDefault();
        return;
      }
      // Cancel, or a click outside the card, closes it.
      if (e.target === dialog || e.target.closest("#cancel")) {
        e.preventDefault();
        closeDialog();
      }
    });
  }
  // What in a row is its own target, so a click there isn't the row's.
  const OWN_TARGET = "a, button, textarea, input, select, label, summary, .rb, .pp, .a";
  // Clicking a row, not one of its own targets, selects it; on the index it
  // also opens the PR, as its title does, and a middle click or one with
  // Ctrl, Cmd or Shift opens it in a new tab. Dragging to select text opens
  // nothing.
  function rowClick(e) {
    const row = e.target.closest("[data-row], .draft");
    if (!row || e.target.closest(OWN_TARGET)) return;
    const selection = window.getSelection();
    if (selection && !selection.isCollapsed && selection.toString().trim()) return;
    const href = page === "index" && row.dataset.href;
    if (e.type === "auxclick") {
      if (e.button !== 1 || !href) return;
      e.preventDefault();
      window.open(href, "_blank", "noopener");
      return;
    }
    if (e.button !== 0) return;
    if (href && (e.ctrlKey || e.metaKey || e.shiftKey)) {
      window.open(href, "_blank", "noopener");
      return;
    }
    lists().forEach(function (list, i) {
      if (list.contains(row)) {
        focus = i;
        selected[i] = idOf(row);
      }
    });
    draw(false);
    if (href && !e.altKey) go(href);
  }
  document.addEventListener("click", rowClick);
  document.addEventListener("auxclick", rowClick);
  // Copy buttons put their command on the clipboard.
  document.addEventListener("click", function (e) {
    const button = e.target.closest("button[data-copy]");
    if (!button) return;
    const source = document.querySelector(button.dataset.copy);
    if (!source || !navigator.clipboard) {
      say("Select the command and copy it");
      return;
    }
    navigator.clipboard.writeText(source.textContent.trim()).then(
      function () {
        say("Copied");
      },
      function () {
        say("Couldn't copy; select the command and copy it");
      }
    );
  });
  // Coming back to a page after confirming in its dialog finds it closed,
  // not showing a spent card.
  window.addEventListener("pageshow", function (e) {
    if (e.persisted && dialogOpen()) closeDialog();
  });
  // A confirm is sent once: a second click or `y` would replace the result
  // page with "Not posted", or post a second empty approval.
  function sendOnce(form) {
    if (!form) return;
    form.addEventListener("submit", function (e) {
      if (form.dataset.sent) {
        e.preventDefault();
        return;
      }
      form.dataset.sent = "true";
      form.querySelectorAll("button").forEach(function (b) {
        b.disabled = true;
      });
    });
  }
  const confirmForm = page === "confirm" && document.getElementById("confirm");
  sendOnce(confirmForm);
  // A form marked data-resend is safe to send again, so coming back to it
  // (say, to fix what was refused) takes it afresh.
  if (confirmForm && "resend" in confirmForm.dataset) {
    window.addEventListener("pageshow", function () {
      delete confirmForm.dataset.sent;
      confirmForm.querySelectorAll("button").forEach(function (b) {
        b.disabled = false;
      });
    });
  }
  // htmx doesn't swap in error pages, so say something went wrong.
  document.body.addEventListener("htmx:responseError", function (e) {
    say("That failed (" + e.detail.xhr.status + "); reload the page to see why.");
  });
  // The groups you've unfolded, by id: the index's refresh swaps in its
  // lists folded, so they're unfolded again as you left them.
  const unfolded = new Set();
  document.addEventListener(
    "toggle",
    function (e) {
      const group = e.target;
      if (!(group instanceof HTMLDetailsElement) || !group.matches("details.grp[id], details.fmore[id]")) return;
      if (group.open) unfolded.add(group.id);
      else unfolded.delete(group.id);
    },
    true
  );
  // A popover v, d or Tab opened, by its id, so the refresh can open it
  // again on the row that comes back.
  let reopen = null;
  // Each list's rows as they were before the index's lists swap, so a
  // selected PR the swap takes away passes to the nearest one left.
  let before = null;
  // The filter box or queue button that had focus, by its id: htmx puts
  // focus back before the groups unfold, and K or J needs the same button
  // under the cursor to walk a run without chasing it.
  let refocus = null;
  document.body.addEventListener("htmx:beforeSwap", function (e) {
    const active = document.activeElement;
    reopen = popover(active) ? active.getAttribute("aria-describedby") : null;
    const queue = document.getElementById("queue");
    const keep =
      active && active.id && (active.closest("#filter") || queue?.contains(active));
    refocus = keep ? active.id : null;
    // Leave a reorder alone: the poll would swap the list out from under
    // the buttons. The next tick is seconds away and brings fresher HTML.
    const poll = queue && e.detail.target === queue && e.detail.elt === queue;
    if (poll && (refocus || queue.querySelector(".a:hover"))) {
      e.detail.shouldSwap = false;
    }
    if (page === "index" && e.detail.target && e.detail.target.id === "panes") {
      before = lists().map(function (list) {
        return rows(list).map(idOf);
      });
    }
  });
  // A selected PR that's gone, filtered out or archived, passes to the
  // next row still shown in its list, else the one before it, else the
  // list's first.
  function keepNearest(old) {
    lists().forEach(function (list, i) {
      if (selected[i] === undefined) return;
      const now = rows(list).map(idOf);
      if (now.indexOf(selected[i]) >= 0) return;
      const was = old[i] || [];
      const at = was.indexOf(selected[i]);
      const around = was.slice(at + 1).concat(was.slice(0, Math.max(at, 0)).reverse());
      const next = around.find(function (key) {
        return now.indexOf(key) >= 0;
      });
      selected[i] = next !== undefined ? next : now[0];
    });
  }
  document.body.addEventListener("htmx:afterSwap", function (e) {
    unfolded.forEach(function (id) {
      const group = document.getElementById(id);
      if (group) group.open = true;
    });
    const was = refocus && document.getElementById(refocus);
    if (was && was !== document.activeElement) was.focus({ preventScroll: true });
    refocus = null;
    localTimes(e.detail.target.isConnected ? e.detail.target : document);
    const pop = reopen && document.getElementById(reopen);
    if (pop && pop.parentElement) pop.parentElement.focus({ preventScroll: true });
    reopen = null;
    const again = refocus && document.getElementById(refocus);
    if (again && !again.disabled) again.focus({ preventScroll: true });
    refocus = null;
    // One shown by hover came back unpinned, so the row would clip it.
    document.querySelectorAll(".rb:hover, .pp:hover").forEach(place);
  });
  // The index rereads its lists every few seconds; keep the selection.
  // A PR page's draft card comes back alone, so its tally is recounted.
  document.body.addEventListener("htmx:afterSettle", function () {
    if (before) keepNearest(before);
    before = null;
    draw(false);
    retally();
  });

  // The PR page's view of its drafts, remembered for this browser: a
  // page asked for without one shows the one you last picked, and the
  // URL says which, so a link to it shows the same.
  const VIEW_KEY = "sanic-review.view";
  function remembered(key) {
    try {
      return window.localStorage.getItem(key);
    } catch (err) {
      return null;
    }
  }
  function remember(key, value) {
    try {
      window.localStorage.setItem(key, value);
    } catch (err) {
      // Private windows may refuse; it's only a convenience.
    }
  }
  // The index's tab, the list you move in, remembered the same way; the
  // page's style shows only the list named on <html>, which refreshes keep.
  const LIST_KEY = "sanic-review.list";
  function markTabs() {
    const shown = document.documentElement.dataset.tab;
    document.querySelectorAll(".tab[data-tab]").forEach(function (tab) {
      tab.setAttribute("aria-pressed", String(tab.dataset.tab === shown));
    });
  }
  function showTab(i) {
    const list = lists()[i];
    if (!list) return;
    focus = i;
    document.documentElement.dataset.tab = list.id;
    markTabs();
  }
  function setTab(i) {
    showTab(i);
    remember(LIST_KEY, document.documentElement.dataset.tab);
    draw(true);
  }
  if (page === "index") {
    const saved = remembered(LIST_KEY);
    const at = lists().findIndex(function (list) {
      return list.id === saved;
    });
    showTab(Math.max(at, 0));
    document.body.addEventListener("htmx:afterSwap", markTabs);
    document.addEventListener("click", function (e) {
      const tab = e.target.closest(".tab[data-tab]");
      if (!tab) return;
      const at = lists().findIndex(function (list) {
        return list.id === tab.dataset.tab;
      });
      if (at < 0) return;
      setTab(at);
      // A click leaves focus on the page, so Enter opens the selected row.
      if (e.detail !== 0) tab.blur();
    });
  }
  // The files view's layout, remembered the same way.
  const LAYOUT_KEY = "sanic-review.layout";
  if (page === "pr") {
    const params = new URLSearchParams(window.location.search);
    const want = new URLSearchParams(params);
    if (!params.has("view") && remembered(VIEW_KEY) === "files") want.set("view", "files");
    if (want.get("view") === "files" && !params.has("layout") && remembered(LAYOUT_KEY) === "split") {
      want.set("layout", "split");
    }
    // Only a page with both views has a view to pick.
    if (document.getElementById("views") && want.toString() !== params.toString()) {
      window.location.replace(window.location.pathname + "?" + want.toString() + window.location.hash);
    }
    document.addEventListener("click", function (e) {
      const tab = e.target.closest("#views a[data-view]");
      if (tab) remember(VIEW_KEY, tab.dataset.view);
      const layout = e.target.closest("#layouts a[data-layout]");
      if (layout) remember(LAYOUT_KEY, layout.dataset.layout);
    });
  }

  // The files view's files: each folds, and ticking one viewed folds it
  // and keeps it folded next time, for this PR at this head.
  const files = page === "pr" && document.querySelector("#drafts.files");
  if (files) {
    const viewedKey = "sanic-review.viewed:" + files.dataset.viewed;
    const viewed = new Set();
    try {
      JSON.parse(remembered(viewedKey) || "[]").forEach(function (path) {
        viewed.add(path);
      });
    } catch (err) {
      // Something else wrote it; start over.
    }
    const fold = function (file, folded) {
      file.classList.toggle("folded", folded);
      const button = file.querySelector(".fold");
      if (button) button.setAttribute("aria-expanded", String(!folded));
    };
    const treeLink = function (file) {
      return files.querySelector('.tree a[href="#' + file.id + '"]');
    };
    files.querySelectorAll(".file").forEach(function (file) {
      const done = viewed.has(file.dataset.path);
      const box = file.querySelector(".viewed input");
      if (box) box.checked = done;
      const link = treeLink(file);
      if (link) link.classList.toggle("done", done);
      if (done) fold(file, true);
    });
    files.addEventListener("click", function (e) {
      const button = e.target.closest(".file .fold");
      if (button) {
        const file = button.closest(".file");
        fold(file, !file.classList.contains("folded"));
        draw(false);
        return;
      }
      // Jumping to a folded file unfolds it.
      const link = e.target.closest(".tree a[href^='#']");
      const target = link && document.getElementById(link.getAttribute("href").slice(1));
      if (target) fold(target, false);
    });
    files.addEventListener("change", function (e) {
      const box = e.target.closest(".file .viewed input");
      if (!box) return;
      const file = box.closest(".file");
      if (box.checked) viewed.add(file.dataset.path);
      else viewed.delete(file.dataset.path);
      remember(viewedKey, JSON.stringify(Array.from(viewed)));
      const link = treeLink(file);
      if (link) link.classList.toggle("done", box.checked);
      fold(file, box.checked);
      draw(false);
    });
    // On a narrow window the file list starts folded, above the files.
    const tree = files.querySelector(".tree");
    if (tree && window.matchMedia("(max-width: 1000px)").matches) tree.open = false;
  }

  // A click in the filter's sidebar sends it and leaves the keys with the
  // lists: nothing there keeps focus but the text box. A key's click, such
  // as Space on a box you tabbed to, has no detail, and keeps it.
  const filterForm = document.getElementById("filter");
  if (filterForm) {
    filterForm.addEventListener("click", function (e) {
      if (e.detail === 0 || e.target.closest("#fq")) return;
      setTimeout(function () {
        const active = document.activeElement;
        if (active && filterForm.contains(active) && active.id !== "fq") active.blur();
      });
    });
  }
  // Clearing the filter in place keeps the selection, as ticking does.
  document.addEventListener("click", function (e) {
    const clear = e.target.closest("a.fclear");
    if (!clear || !filterForm || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey) return;
    e.preventDefault();
    filterForm.querySelectorAll("input[type=checkbox]").forEach(function (box) {
      box.checked = false;
    });
    // Words are emptied as typing would, so htmx's `changed` knows the box
    // is empty and the same words typed again are sent.
    const box = document.getElementById("fq");
    if (box.value) {
      box.value = "";
      htmx.trigger(box, "input");
    } else {
      htmx.trigger(filterForm, "change");
    }
  });

  // The filter's sidebar: on a narrow window it starts folded, over the
  // lists; on a wide one it stays open.
  const fold = document.getElementById("filter-fold");
  if (fold) {
    const narrow = window.matchMedia("(max-width: 1000px)");
    if (narrow.matches) fold.open = false;
    // Widened past it, the sidebar opens again, since there its summary
    // no longer toggles it; narrowed past it, it folds, as it starts,
    // unless focus is in it: folding would drop focus to the page, and
    // the next letter typed would be a key to the lists.
    narrow.addEventListener("change", function () {
      if (!narrow.matches) fold.open = true;
      else if (!fold.contains(document.activeElement)) fold.open = false;
    });
    fold.querySelector("summary").addEventListener("click", function (e) {
      if (!narrow.matches) e.preventDefault();
    });
  }

  function retally() {
    document.querySelectorAll("#review-bar [data-count]").forEach(function (b) {
      b.textContent = document.querySelectorAll("#drafts .draft." + b.dataset.count).length;
    });
  }
})();
