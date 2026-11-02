// Times on the dashboard (design specification section 11.8): each
// <time datetime> element carries the full UTC time, shown as the
// viewer's local clock with the full time on hover. Fragments that htmx
// swaps in are converted as they arrive.

(function () {
  "use strict";

  function pad(number, width) {
    return String(number).padStart(width, "0");
  }

  function localise(root) {
    root.querySelectorAll("time[datetime]:not([data-local])").forEach(function (element) {
      var when = new Date(element.getAttribute("datetime"));

      if (isNaN(when.getTime())) {
        return;
      }

      element.textContent = pad(when.getHours(), 2) + ":" + pad(when.getMinutes(), 2) + ":" +
        pad(when.getSeconds(), 2) + "." + pad(when.getMilliseconds(), 3);
      element.title = element.getAttribute("datetime");
      element.setAttribute("data-local", "");
    });
  }

  document.addEventListener("DOMContentLoaded", function () { localise(document); });
  document.addEventListener("htmx:afterSwap", function (event) { localise(event.target); });
  document.addEventListener("htmx:sseMessage", function () { localise(document); });
})();
