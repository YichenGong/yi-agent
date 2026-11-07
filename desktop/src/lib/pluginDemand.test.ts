import { describe, expect, it } from "vitest";
import {
  aggregateDemand,
  consumerDelta,
  demandDelta,
  projectsWithPluginViews,
  type PluginConsumer,
} from "./pluginDemand";

const consumer = (consumer: string, plugin = "board", project = "/p/a"): PluginConsumer => ({
  project,
  plugin,
  consumer,
});

describe("aggregateDemand", () => {
  it("folds two views of one plugin into one intent", () => {
    expect(aggregateDemand([consumer("v1"), consumer("v2")])).toEqual([
      { project: "/p/a", plugin: "board", consumers: ["v1", "v2"] },
    ]);
  });

  it("keeps the same plugin name apart across projects", () => {
    const intents = aggregateDemand([consumer("a"), consumer("b", "board", "/p/b")]);
    expect(intents).toHaveLength(2);
    expect(intents.map((intent) => intent.project)).toEqual(["/p/a", "/p/b"]);
  });

  it("names each consumer once even if it is reported twice", () => {
    expect(aggregateDemand([consumer("v1"), consumer("v1")])[0].consumers).toEqual(["v1"]);
  });
});

describe("demandDelta", () => {
  it("acquires a plugin nobody had", () => {
    expect(demandDelta([consumer("v1")], []).acquire).toEqual([
      { project: "/p/a", plugin: "board", consumers: ["v1"] },
    ]);
    expect(demandDelta([consumer("v1")], []).release).toEqual([]);
  });

  it("registers a second consumer without re-acquiring for the first", () => {
    // The second view has to be known locally: otherwise its later withdrawal
    // would look like the last consumer leaving and stop a service still in use.
    // Only the joining consumer is sent -- the shell already counts the first.
    const delta = demandDelta([consumer("v1"), consumer("v2")], [consumer("v1")]);
    expect(delta.acquire).toEqual([
      { project: "/p/a", plugin: "board", consumers: ["v2"] },
    ]);
    expect(delta.release).toEqual([]);
  });

  it("withdraws exactly the consumer that left, not the whole plugin", () => {
    // The native side counts consumers and only stops the service when the last one
    // is gone. Sending "release the plugin" for one view's exit would stop a service
    // another view still uses, so the departing consumer is named.
    const oneLeft = demandDelta([consumer("v2")], [consumer("v1"), consumer("v2")]);
    expect(oneLeft.acquire).toEqual([]);
    expect(oneLeft.release).toEqual([
      { project: "/p/a", plugin: "board", consumers: ["v1"] },
    ]);

    const noneLeft = demandDelta([], [consumer("v2")]);
    expect(noneLeft.release).toEqual([{ project: "/p/a", plugin: "board", consumers: ["v2"] }]);
  });

  it("is empty when nothing changed", () => {
    expect(demandDelta([consumer("v1")], [consumer("v1")])).toEqual({ acquire: [], release: [] });
  });
});

describe("consumerDelta", () => {
  it("names each joining view, so the shell can count them", () => {
    expect(consumerDelta([consumer("v1"), consumer("v2")], [consumer("v1")])).toEqual({
      acquire: [consumer("v2")],
      release: [],
    });
  });

  it("withdraws only the view that unmounted", () => {
    expect(consumerDelta([consumer("v2")], [consumer("v1"), consumer("v2")])).toEqual({
      acquire: [],
      release: [consumer("v1")],
    });
  });

  it("does not confuse the same view name in two plugins", () => {
    const delta = consumerDelta([consumer("main", "board")], [consumer("main", "notes")]);
    expect(delta.acquire.map((c) => c.plugin)).toEqual(["board"]);
    expect(delta.release.map((c) => c.plugin)).toEqual(["notes"]);
  });

  it("does not confuse the same view name in two projects", () => {
    const delta = consumerDelta(
      [consumer("main", "board", "/p/a")],
      [consumer("main", "board", "/p/b")],
    );
    expect(delta.acquire.map((c) => c.project)).toEqual(["/p/a"]);
    expect(delta.release.map((c) => c.project)).toEqual(["/p/b"]);
  });

  it("is empty when nothing changed", () => {
    expect(consumerDelta([consumer("v1")], [consumer("v1")])).toEqual({
      acquire: [],
      release: [],
    });
  });
});

describe("projectsWithPluginViews", () => {
  it("makes a board-only project wanted before any thread exists", () => {
    expect(projectsWithPluginViews([consumer("v1", "board", "/p/a")])).toEqual(["/p/a"]);
  });

  it("deduplicates projects with several plugin views", () => {
    expect(
      projectsWithPluginViews([consumer("v1", "board"), consumer("v2", "notes")]),
    ).toEqual(["/p/a"]);
  });
});
