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
    const drafts = document.getElementById("drafts");
    return drafts ? [drafts] : [];
  }

  // The rows you can move to: a folded group's are skipped.
  function rows(list) {
    const all = list.querySelectorAll(page === "index" ? "[data-row]" : ".draft");
    return Array.from(all).filter(function (row) {
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
        // Confirmed from the index, it comes back to the index.
        const next = card.querySelector('input[name="next"]');
        if (next && page === "index") next.value = "index";
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
      // Esc leaves a draft's text box, which saves it.
      if (e.key === "Escape" && !e.repeat) e.target.blur();
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
        focus = (focus + (e.shiftKey ? n - 1 : 1)) % n;
        draw(true);
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
        if (page === "index") go(row.dataset.href);
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
      case "X": {
        const panes = document.getElementById("panes");
        const shown = panes && panes.dataset.showArchived === "true";
        go("/?archived=" + !shown);
        break;
      }
      case "c": {
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
      if (!(group instanceof HTMLDetailsElement) || !group.matches("details.grp[id]")) return;
      if (group.open) unfolded.add(group.id);
      else unfolded.delete(group.id);
    },
    true
  );
  // A popover v, d or Tab opened, by its id, so the refresh can open it
  // again on the row that comes back.
  let reopen = null;
  document.body.addEventListener("htmx:beforeSwap", function () {
    const active = document.activeElement;
    reopen = popover(active) ? active.getAttribute("aria-describedby") : null;
  });
  document.body.addEventListener("htmx:afterSwap", function (e) {
    unfolded.forEach(function (id) {
      const group = document.getElementById(id);
      if (group) group.open = true;
    });
    localTimes(e.detail.target.isConnected ? e.detail.target : document);
    const pop = reopen && document.getElementById(reopen);
    if (pop && pop.parentElement) pop.parentElement.focus({ preventScroll: true });
    reopen = null;
    // One shown by hover came back unpinned, so the row would clip it.
    document.querySelectorAll(".rb:hover, .pp:hover").forEach(place);
  });
  // The index rereads its lists every few seconds; keep the selection.
  // A PR page's draft card comes back alone, so its tally is recounted.
  document.body.addEventListener("htmx:afterSettle", function () {
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

  function retally() {
    document.querySelectorAll("#review-bar [data-count]").forEach(function (b) {
      b.textContent = document.querySelectorAll("#drafts .draft." + b.dataset.count).length;
    });
  }
})();
