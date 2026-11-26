const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const { JSDOM } = require("jsdom");

const html = fs.readFileSync(path.join(__dirname, "..", "assets", "scheduler.html"), "utf8");
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];

function pad(n) { return String(n).padStart(2, "0"); }
function keyOf(d) { return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate()); }
const DAY_KEYS = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

function boot() {
  const dom = new JSDOM(html, { url: "https://aalu.test/scheduler/index.html", runScripts: "outside-only", pretendToBeVisual: true });
  const messages = [];
  dom.window.ipc = { postMessage: (m) => messages.push(JSON.parse(m)) };
  dom.window.eval(script);
  const week = {};
  for (const k of DAY_KEYS) week[k] = "t1";
  dom.window.__sched.load({
    version: 1,
    templates: [{ id: "t1", name: "Every day", blocks: [
      { id: "b1", time: "06:00", text: "Padhai" },
      { id: "b2", time: "07:30", text: "Exercise" },
    ] }],
    week,
    done: {},
  });
  return { window: dom.window, document: dom.window.document, messages };
}

test("checklist tab shows today's blocks and ticking one saves it", () => {
  const app = boot();
  app.document.getElementById("tabCheck").click();
  assert.equal(app.document.getElementById("checkView").classList.contains("view-hidden"), false);
  assert.equal(app.document.getElementById("planView").classList.contains("view-hidden"), true);
  const rows = app.document.querySelectorAll(".check-row");
  assert.equal(rows.length, 2);
  assert.match(app.document.getElementById("checkCount").textContent, /0 \/ 2 done/);

  rows[0].click();
  const msg = app.messages.filter((m) => m.t === "check").at(-1);
  assert.deepEqual(msg, { t: "check", date: keyOf(new Date()), id: "b1", done: true });
  assert.match(app.document.getElementById("checkCount").textContent, /1 \/ 2 done/);
  assert.ok(app.document.querySelector(".check-row.done"));
  assert.equal(app.document.getElementById("progressFill").style.width, "50%");

  app.document.querySelector(".check-row.done").click();
  assert.equal(app.messages.filter((m) => m.t === "check").at(-1).done, false);
  assert.match(app.document.getElementById("checkCount").textContent, /0 \/ 2 done/);
  app.window.close();
});

test("checklist is per day: yesterday is independent of today", () => {
  const app = boot();
  app.document.getElementById("tabCheck").click();
  app.document.querySelector(".check-row").click(); // tick today's first block
  app.document.getElementById("datePrev").click();
  assert.match(app.document.getElementById("checkCount").textContent, /0 \/ 2 done/);
  app.document.getElementById("dateToday").click();
  assert.match(app.document.getElementById("checkCount").textContent, /1 \/ 2 done/);
  app.window.close();
});

test("a weekday without a template shows a hint instead of a checklist", () => {
  const app = boot();
  const data = JSON.parse(JSON.stringify({ version: 1, templates: [], week: {}, done: {} }));
  app.window.__sched.load(data);
  app.document.getElementById("tabCheck").click();
  assert.equal(app.document.querySelectorAll(".check-row").length, 0);
  assert.match(app.document.querySelector(".empty-state").textContent, /No template is assigned/);
  app.window.close();
});
