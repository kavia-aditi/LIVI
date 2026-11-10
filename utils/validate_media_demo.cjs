"use strict";

// Run from the project root: node --test utils/validate_media_demo.cjs
// The minimal DOM harness checks behavior, not browser rendering or layout.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const { test } = require("node:test");

const root = path.resolve(__dirname, "..");
const htmlPath = path.join(root, "demos", "media-routing.html");
const html = fs.readFileSync(htmlPath, "utf8");
const scripts = [...html.matchAll(/<script id="([^"]+)">([\s\S]*?)<\/script>/g)];
const modelCode = scripts.find(match => match[1] === "demo-model")?.[2];
const uiCode = scripts.find(match => match[1] === "demo-ui")?.[2];

function model() {
  assert.ok(modelCode, "Demo model script must exist");
  const context = vm.createContext({});
  vm.runInContext(modelCode, context, { filename: "demo-model", timeout: 1000 });
  return context.MediaDemo;
}

class Element {
  constructor(id = "") {
    this.id = id;
    this.textContent = "";
    this.value = "";
    this.disabled = false;
    this.hidden = false;
    this.dataset = {};
    this.style = {};
    this.attributes = {};
    this.listeners = new Map();
    this.children = [];
    const classes = new Set();
    this.classList = {
      toggle(name, force) {
        const enabled = force === undefined ? !classes.has(name) : Boolean(force);
        if (enabled) classes.add(name);
        else classes.delete(name);
        return enabled;
      },
      contains: name => classes.has(name)
    };
  }

  setAttribute(name, value) {
    this.attributes[name] = String(value);
  }

  addEventListener(name, callback) {
    const callbacks = this.listeners.get(name) || [];
    callbacks.push(callback);
    this.listeners.set(name, callbacks);
  }

  replaceChildren(...children) {
    this.children = children;
  }

  dispatch(name, event = {}) {
    if (name === "click" && this.disabled) return;
    for (const callback of this.listeners.get(name) || []) {
      callback({ target: this, ...event });
    }
  }

  click() {
    this.dispatch("click");
  }
}

function browser(scenario = "dab") {
  assert.ok(uiCode, "Demo UI script must exist");
  const elements = new Map(
    [...html.matchAll(/\bid="([^"]+)"/g)].map(match => [
      match[1], new Element(match[1])
    ])
  );
  const buttons = [...html.matchAll(/data-scenario="([^"]+)"/g)].map(match => {
    const button = new Element();
    button.dataset.scenario = match[1];
    return button;
  });
  const document = new Element("document");
  document.getElementById = id => {
    assert.ok(elements.has(id), `Unknown element: ${id}`);
    return elements.get(id);
  };
  document.querySelectorAll = selector => {
    assert.equal(selector, "[data-scenario]");
    return buttons;
  };
  document.createElement = tag => {
    assert.equal(tag, "li");
    return new Element();
  };
  const context = vm.createContext({
    document,
    location: { search: `?scenario=${encodeURIComponent(scenario)}` },
    URLSearchParams
  });
  vm.runInContext(modelCode, context, { filename: "demo-model", timeout: 1000 });
  vm.runInContext(uiCode, context, { filename: "demo-ui", timeout: 1000 });
  return {
    get: id => document.getElementById(id),
    choose(name) {
      const button = buttons.find(item => item.dataset.scenario === name);
      assert.ok(button, `Unknown scenario: ${name}`);
      button.click();
    },
    key(overrides = {}) {
      let prevented = false;
      document.dispatch("keydown", {
        code: "KeyP",
        repeat: false,
        ctrlKey: false,
        metaKey: false,
        altKey: false,
        preventDefault() { prevented = true; },
        ...overrides
      });
      return prevented;
    },
    api: context.MediaDemo
  };
}

test("both deliverable files exist", () => {
  assert.ok(fs.statSync(htmlPath).isFile());
  assert.ok(fs.statSync(__filename).isFile());
});

test("scripts compile, IDs are unique, links resolve, simulation is explicit", () => {
  assert.equal(scripts.length, 2);
  for (const [, name, code] of scripts) {
    new vm.Script(code, { filename: name });
  }
  const ids = [...html.matchAll(/\bid="([^"]+)"/g)].map(match => match[1]);
  assert.equal(new Set(ids).size, ids.length);
  assert.match(html, /Interactive simulation/);
  assert.match(html, /not the native application/);
  assert.doesNotMatch(html, /<script\b[^>]*\bsrc=/i);
  for (const [, href] of html.matchAll(/href="([^"]+)"/g)) {
    assert.ok(fs.existsSync(path.resolve(path.dirname(htmlPath), href)), href);
  }
});

test("all 21 built-in simulation behavior checks pass", () => {
  assert.equal(model().selfTest(), 21);
});

test("scenario 1: disconnected phone blocks controls and global media key", () => {
  const b = browser("disconnected");
  assert.equal(b.get("track").textContent, "No phone connected");
  for (const id of ["prev", "play", "next", "seek"]) {
    assert.equal(b.get(id).disabled, true, id);
  }
  b.get("send-key").click();
  assert.match(b.get("outcome").textContent, /blocked/);
  assert.equal(b.get("tabs").hidden, true);
});

test("scenario 2: missing metadata shows prompt but keeps connected controls", () => {
  const b = browser("connected");
  assert.equal(b.get("track").textContent, "Start playback on your phone");
  assert.equal(b.get("artist").textContent, "");
  assert.equal(b.get("album").textContent, "");
  assert.equal(b.get("play").disabled, false);
  b.get("play").click();
  assert.equal(b.get("play").attributes["aria-label"], "Pause");
  assert.match(b.get("outcome").textContent, /CarPlay/);
});

test("scenario 3: populated media displays metadata and playback state", () => {
  const b = browser("playing");
  assert.equal(b.get("track").textContent, "Night Drive");
  assert.equal(b.get("artist").textContent, "LIVI Demo Artist");
  assert.match(b.get("album").textContent, /sample metadata/);
  assert.equal(b.get("art").classList.contains("loaded"), true);
  assert.equal(b.get("elapsed").textContent, "0:42");
  b.get("next").click();
  assert.match(b.get("outcome").textContent, /next track/);
});

test("scenario 4: Android Auto supports playback but not seeking", () => {
  const b = browser("android");
  assert.equal(b.get("seek").disabled, true);
  assert.equal(b.get("play").disabled, false);
  b.get("send-key").click();
  assert.match(b.get("outcome").textContent, /Android Auto/);
  const result = b.api.transition(b.api.initial("android"), "seek", 90);
  assert.equal(result.kind, "blocked");
  assert.equal(result.state.elapsed, 42);
});

test("CarPlay-only seeking is wired, bounded, and blocked without a phone", () => {
  const b = browser("playing");
  assert.equal(b.get("seek").disabled, false);
  b.get("seek").value = "90";
  b.get("seek").dispatch("input");
  assert.equal(b.get("elapsed").textContent, "1:30");
  assert.match(b.get("outcome").textContent, /CarPlay seek/);
  assert.equal(
    b.api.transition(b.api.initial("playing"), "seek", 999).state.elapsed, 180
  );
  assert.equal(
    b.api.transition(b.api.initial("disconnected"), "seek", 90).kind, "blocked"
  );
});

test("scenario 5: idle DAB resumes from phone tab without enabling phone controls", () => {
  const b = browser("dab");
  assert.equal(b.get("phone-tab").attributes["aria-pressed"], "true");
  assert.match(b.get("dab-fact").textContent, /stopped/);
  b.get("send-key").click();
  assert.match(b.get("outcome").textContent, /resume DAB/);
  assert.match(b.get("dab-fact").textContent, /playing/);
  assert.equal(b.get("phone-tab").attributes["aria-pressed"], "true");
  assert.equal(b.get("play").disabled, true);
  b.get("send-key").click();
  assert.match(b.get("dab-fact").textContent, /paused/);
  b.get("dab-tab").click();
  assert.equal(b.get("play").disabled, false);
  b.get("phone-tab").click();
  assert.equal(b.get("play").disabled, true);
  assert.match(b.get("dab-fact").textContent, /stopped/);
});

test("scenario 6: shared key routes to cluster layout, not media", () => {
  const b = browser("collision");
  b.get("send-key").click();
  assert.equal(b.get("layout").textContent, "Cluster layout 2 / 3");
  assert.match(b.get("outcome").textContent, /No media action sent/);
  assert.equal(b.get("play").disabled, true);
  assert.equal(b.key(), true);
  assert.equal(b.get("layout").textContent, "Cluster layout 3 / 3");
  for (const override of [
    { repeat: true }, { ctrlKey: true }, { metaKey: true }, { altKey: true },
    { code: "KeyQ" }
  ]) {
    assert.equal(b.key(override), false);
    assert.equal(b.get("layout").textContent, "Cluster layout 3 / 3");
  }
});

test("all six scenario buttons, reset, and demo-check button are wired", () => {
  const b = browser();
  assert.equal(Object.keys(b.api.scenarios).length, 6);
  for (const name of Object.keys(b.api.scenarios)) {
    b.choose(name);
    assert.equal(b.get("scenario-title").textContent, b.api.scenarios[name].title);
  }
  b.get("send-key").click();
  b.get("reset").click();
  assert.equal(b.get("layout").textContent, "Cluster layout 1 / 3");
  b.get("checks").click();
  assert.equal(
    b.get("check-result").textContent,
    "21 demo checks passed (simulation only)."
  );
});

test("unknown scenario falls back safely and event history stays bounded", () => {
  const b = browser("not-a-scenario");
  assert.equal(b.get("scenario-title").textContent, b.api.scenarios.dab.title);
  for (let i = 0; i < 10; i++) b.get("send-key").click();
  assert.equal(b.get("events").children.length, 6);
});
