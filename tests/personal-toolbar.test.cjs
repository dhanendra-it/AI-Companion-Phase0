const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const { JSDOM } = require("jsdom");

const html = fs.readFileSync(path.join(__dirname, "..", "assets", "personal-toolbar.html"), "utf8");
const scriptMatch = html.match(/<script>([\s\S]*?)<\/script>/);
assert.ok(scriptMatch, "toolbar script must exist");

function boot(saved = {}) {
  const dom = new JSDOM(html, {
    url: "https://aalu.test/toolbar/index.html",
    runScripts: "outside-only",
    pretendToBeVisual: true,
  });
  const messages = [];
  dom.window.ipc = { postMessage: (message) => messages.push(message) };
  dom.window.confirm = () => true;
  for (const [key, value] of Object.entries(saved)) {
    dom.window.localStorage.setItem(key, JSON.stringify(value));
  }
  dom.window.eval(scriptMatch[1]);
  return { dom, window: dom.window, document: dom.window.document, messages };
}

function clickLabel(document, label) {
  const slot = [...document.querySelectorAll(".slot")]
    .find((element) => element.querySelector(".lbl")?.textContent.trim() === label);
  assert.ok(slot, "expected toolbar item " + JSON.stringify(label) + " to exist");
  slot.click();
  return slot;
}

function addItem(document, { section, name, target, kind }) {
  clickLabel(document, section);
  clickLabel(document, "Add");
  document.getElementById("name").value = name;
  document.getElementById("target").value = target;
  document.getElementById("kind").value = kind;
  document.getElementById("kind").dispatchEvent(new document.defaultView.Event("change"));
  document.getElementById("save").click();
}

test("text snippets preserve multiline Unicode text and use native clipboard", () => {
  const app = boot();
  const snippet = "नमस्ते — hello\nSecond line: 100% & done";
  addItem(app.document, { section: "Text Snippets", name: "Greeting", target: snippet, kind: "text" });
  assert.equal(JSON.parse(app.window.localStorage.getItem("aalu.personalToolbar.v1")).Commands[0].target, snippet);
  clickLabel(app.document, "Greeting");
  const message = app.messages.map((x) => { try { return JSON.parse(x); } catch { return x; } })
    .find((x) => x && x.t === "copy-text");
  assert.equal(message.target, snippet);
  app.dom.window.close();
});

test("saved entries survive a WebView reload", () => {
  const first = boot();
  addItem(first.document, { section: "Bookmarks", name: "Example", target: "https://example.com", kind: "url" });
  const saved = JSON.parse(first.window.localStorage.getItem("aalu.personalToolbar.v1"));
  first.dom.window.close();
  const second = boot({ "aalu.personalToolbar.v1": saved });
  clickLabel(second.document, "Bookmarks");
  assert.ok([...second.document.querySelectorAll(".slot .lbl")].some((x) => x.textContent.trim() === "Example"));
  second.dom.window.close();
});

test("bookmarks validate and dispatch website targets", () => {
  const app = boot();
  addItem(app.document, { section: "Bookmarks", name: "Example", target: "example.com", kind: "url" });
  clickLabel(app.document, "Example");
  const message = app.messages.map((x) => { try { return JSON.parse(x); } catch { return x; } })
    .find((x) => x && x.t === "open-url");
  assert.equal(message.target, "https://example.com");
  app.dom.window.close();
});

test("folder picker requests folder mode and launches the selected folder", () => {
  const app = boot();
  clickLabel(app.document, "Folders");
  clickLabel(app.document, "Add");
  app.document.getElementById("name").value = "Documents";
  app.document.getElementById("pickFolder").click();
  const picker = app.messages.map((x) => { try { return JSON.parse(x); } catch { return x; } })
    .find((x) => x && x.t === "pick");
  assert.equal(picker.mode, "folder");
  app.window.__picked("C:\\Users\\Test\\Documents");
  app.document.getElementById("save").click();
  clickLabel(app.document, "Documents");
  const launch = app.messages.map((x) => { try { return JSON.parse(x); } catch { return x; } })
    .find((x) => x && x.t === "launch-app");
  assert.equal(launch.target, "C:\\Users\\Test\\Documents");
  app.dom.window.close();
});

test("Settings opens Window Manager and its actions reach the native bridge", () => {
  const app = boot();
  clickLabel(app.document, "Settings");
  clickLabel(app.document, "Window Manager");
  clickLabel(app.document, "Open chat");
  assert.ok(app.messages.includes("open-chat"));
  app.dom.window.close();
});

test("removing an item updates persisted data", () => {
  const app = boot();
  addItem(app.document, { section: "Text Snippets", name: "Temporary", target: "remove me", kind: "text" });
  const item = clickLabel(app.document, "Temporary");
  item.dispatchEvent(new app.window.MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
  const saved = JSON.parse(app.window.localStorage.getItem("aalu.personalToolbar.v1"));
  assert.equal(saved.Commands.length, 0);
  app.dom.window.close();
});

test("long sections paginate without losing later entries", () => {
  const entries = Array.from({ length: 9 }, (_, i) => ({ name: "Item " + (i + 1), target: "text " + (i + 1), kind: "text" }));
  const app = boot({ "aalu.personalToolbar.v1": { Commands: entries } });
  clickLabel(app.document, "Text Snippets");
  assert.match(app.document.getElementById("center").textContent, /Page 1\/2/);
  app.document.dispatchEvent(new app.window.WheelEvent("wheel", { deltaY: 120, bubbles: true }));
  assert.match(app.document.getElementById("center").textContent, /Page 2\/2/);
  clickLabel(app.document, "Item 9");
  const message = app.messages.map((x) => { try { return JSON.parse(x); } catch { return x; } })
    .find((x) => x && x.t === "copy-text");
  assert.equal(message.target, "text 9");
  app.dom.window.close();
});
