// Keyboard shortcuts for the dashboard: the TUI's keys, where it has them.
// No framework; the page works without this file, just without keys.
(function () {
  "use strict";

  const page = document.body.dataset.page;
  const help = document.getElementById("help");
  const notice = document.getElementById("notice");
  const RERUN_HINT = "r reviews a failed, held, skipped or archived PR you owe now";

  // Lists you move in: the index's two panes, or a PR page's drafts.
  let focus = 0;
  // The selected row in each list, by position; none until you move.
  const selected = [];

  function lists() {
    if (page === "index") return Array.from(document.querySelectorAll(".pane"));
    const drafts = document.getElementById("drafts");
    return drafts ? [drafts] : [];
  }

  function rows(list) {
    return Array.from(list.querySelectorAll(page === "index" ? ".row" : ".draft"));
  }

  function currentRow() {
    const list = lists()[focus];
    if (!list || selected[focus] === undefined) return null;
    return rows(list)[selected[focus]] || null;
  }

  function draw(scroll) {
    lists().forEach(function (list, i) {
      list.classList.toggle("focused", i === focus && page === "index");
      const all = rows(list);
      if (selected[i] !== undefined) selected[i] = Math.min(selected[i], all.length - 1);
      all.forEach(function (row, j) {
        row.classList.toggle("selected", i === focus && j === selected[i]);
      });
    });
    const row = currentRow();
    if (row && scroll) row.scrollIntoView({ block: "nearest" });
  }

  function moveTo(target) {
    const list = lists()[focus];
    if (!list) return;
    const last = rows(list).length - 1;
    if (last < 0) return;
    selected[focus] = Math.max(0, Math.min(target, last));
    draw(true);
  }

  function moveBy(delta) {
    const now = selected[focus];
    moveTo(now === undefined ? 0 : now + delta);
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

  // What r and a act on: the selected row on the index, the PR itself on a
  // PR page.
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

  function onKey(e) {
    // Not even the browser's own handling, like Space ticking a box.
    if (e.repeat || Date.now() - shown < SETTLE_MS) {
      e.preventDefault();
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (typing(e.target)) {
      // Esc leaves a draft's text box, which saves it.
      if (e.key === "Escape") e.target.blur();
      return;
    }
    if (!help.hidden) {
      if (e.key === "?" || e.key === "Escape" || e.key === "q") {
        help.hidden = true;
        e.preventDefault();
      }
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
    switch (e.key) {
      case "?":
        help.hidden = false;
        break;
      case "q":
        if (page === "index") say("q goes back from other pages; close the tab to leave");
        else go("/");
        break;
      case "Tab": {
        const n = lists().length;
        if (page !== "index" || n === 0) return;
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
        else {
          const box = row.querySelector("textarea");
          if (box) box.focus();
        }
        break;
      }
      case "r": {
        const target = subject();
        const href = target && target.dataset.reviewNow;
        if (href) go(href);
        else say(RERUN_HINT);
        break;
      }
      case "a": {
        const target = subject();
        const form = target && target.querySelector("form.archive");
        if (form) form.requestSubmit();
        else say("a archives the selected PR");
        break;
      }
      case "A": {
        const panes = document.getElementById("panes");
        const shown = panes && panes.dataset.showArchived === "true";
        go("/?archived=" + !shown);
        break;
      }
      case "i":
        say("Skipping by title isn't in the dashboard yet: use i in the TUI (serve --ui tui).");
        break;
      default:
        return;
    }
    e.preventDefault();
  }

  document.addEventListener("keydown", onKey);
  // Back and forward can bring a page back with its approval box ticked;
  // it's ticked afresh each time.
  window.addEventListener("pageshow", function () {
    document.querySelectorAll('input[name="approve"]').forEach(function (box) {
      box.checked = false;
    });
  });
  // A confirm is sent once: a second click or `y` would replace the result
  // page with "Not posted", or post a second empty approval.
  const confirmForm = page === "confirm" && document.getElementById("confirm");
  if (confirmForm) {
    confirmForm.addEventListener("submit", function (e) {
      if (confirmForm.dataset.sent) {
        e.preventDefault();
        return;
      }
      confirmForm.dataset.sent = "true";
      confirmForm.querySelectorAll("button").forEach(function (b) {
        b.disabled = true;
      });
    });
  }
  // htmx doesn't swap in error pages, so say something went wrong.
  document.body.addEventListener("htmx:responseError", function (e) {
    say("That failed (" + e.detail.xhr.status + "); reload the page to see why.");
  });
  // The index rereads its lists every few seconds; keep the selection.
  document.body.addEventListener("htmx:afterSettle", function () {
    draw(false);
  });
})();
