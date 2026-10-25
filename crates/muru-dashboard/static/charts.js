// The dashboard's charts (design specification section 10.7): requests
// and errors per second, and the median and 95th percentile of total
// time, over the last hour at one second or the last day at one minute,
// from /v1/history, kept live by the /v1/history/stream events.

(function () {
  "use strict";

  var ranges = { "1h": 3600, "24h": 1440 };
  var range = new URLSearchParams(window.location.search).get("range") === "24h" ? "24h" : "1h";
  var series = { ts: [], rps: [], errors: [], p50: [], p95: [], inflight: [] };
  var charts = [];

  function push(point) {
    series.ts.push(point[0]);
    series.rps.push(point[1]);
    series.errors.push(point[2]);
    series.p50.push(point[3]);
    series.p95.push(point[4]);
    series.inflight.push(point[5]);

    while (series.ts.length > ranges[range]) {
      Object.keys(series).forEach(function (key) { series[key].shift(); });
    }
  }

  function data(keys) {
    return [series.ts].concat(keys.map(function (key) { return series[key]; }));
  }

  function colours() {
    var dark = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
    return {
      axis: dark ? "#9fb3c8" : "#627d98",
      grid: dark ? "#243b53" : "#e4ebf2",
      one: "#2f6fed",
      two: "#b42318",
      three: "#946200",
    };
  }

  function chart(element, title, keys, labels, unit) {
    var c = colours();
    var width = element.clientWidth || 600;
    var options = {
      title: title,
      width: width,
      height: 180,
      cursor: { drag: { x: false, y: false } },
      legend: { show: true },
      scales: { x: { time: true } },
      axes: [
        { stroke: c.axis, grid: { stroke: c.grid } },
        { stroke: c.axis, grid: { stroke: c.grid }, size: 60, values: function (u, values) {
          return values.map(function (v) { return v + unit; });
        } },
      ],
      series: [{}].concat(keys.map(function (key, index) {
        return {
          label: labels[index],
          stroke: [c.one, c.two, c.three][index],
          width: 1.5,
          points: { show: false },
          value: function (u, v) { return v == null ? "-" : (Math.round(v * 10) / 10) + unit; },
        };
      })),
    };
    var plot = new uPlot(options, data(keys), element);
    charts.push({ plot: plot, keys: keys, element: element });
  }

  function redraw() {
    charts.forEach(function (entry) {
      entry.plot.setData(data(entry.keys));
    });
  }

  function resize() {
    charts.forEach(function (entry) {
      entry.plot.setSize({ width: entry.element.clientWidth || 600, height: 180 });
    });
  }

  function start() {
    var section = document.getElementById("charts");

    if (!section || typeof uPlot === "undefined") {
      return;
    }

    section.querySelectorAll("a[data-range]").forEach(function (link) {
      if (link.getAttribute("data-range") === range) {
        link.classList.add("current");
      }
    });

    fetch("/v1/history?range=" + range, { cache: "no-store" })
      .then(function (response) { return response.json(); })
      .then(function (body) {
        body.points.forEach(push);
        chart(document.getElementById("chart-rate"), "Requests and errors per second", ["rps", "errors"], ["requests/s", "errors/s"], "");
        chart(document.getElementById("chart-latency"), "Total time", ["p50", "p95"], ["p50", "p95"], " ms");
        window.addEventListener("resize", resize);

        if (range === "1h" && typeof EventSource !== "undefined") {
          var stream = new EventSource("/v1/history/stream");
          stream.addEventListener("point", function (event) {
            push(JSON.parse(event.data));
            redraw();
          });
        }
      })
      .catch(function (error) {
        section.insertAdjacentText("beforeend", "the history could not be loaded: " + error);
      });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  } else {
    start();
  }
})();
