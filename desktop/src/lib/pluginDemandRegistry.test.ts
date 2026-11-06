import { describe, expect, it, beforeEach } from "vitest";
import {
  pluginConsumersSnapshot,
  registerPluginConsumer,
  resetPluginConsumers,
  subscribePluginConsumers,
} from "./pluginDemandRegistry";

const consumer = (name: string, plugin = "board", project = "/p/a") => ({
  project,
  plugin,
  consumer: name,
});

describe("pluginDemandRegistry", () => {
  beforeEach(() => resetPluginConsumers());

  it("publishes a registration so one subscriber can see it", () => {
    let notified = 0;
    const stop = subscribePluginConsumers(() => {
      notified += 1;
    });
    registerPluginConsumer(consumer("v1"));
    expect(pluginConsumersSnapshot()).toEqual([consumer("v1")]);
    expect(notified).toBe(1);
    stop();
  });

  it("is idempotent for the same consumer", () => {
    // A re-mount that fails to unsubscribe first must not make the count wrong:
    // the count is what decides when a shared service stops.
    const first = registerPluginConsumer(consumer("v1"));
    registerPluginConsumer(consumer("v1"));
    expect(pluginConsumersSnapshot()).toHaveLength(1);
    first();
    expect(pluginConsumersSnapshot()).toEqual([]);
  });

  it("withdraws only the consumer that released", () => {
    const releaseOne = registerPluginConsumer(consumer("v1"));
    registerPluginConsumer(consumer("v2"));
    releaseOne();
    expect(pluginConsumersSnapshot()).toEqual([consumer("v2")]);
  });

  it("ignores a second release of the same consumer", () => {
    // A view can unmount twice through a cleanup path it does not control. The
    // second release must not withdraw somebody else's demand.
    const releaseOne = registerPluginConsumer(consumer("v1"));
    registerPluginConsumer(consumer("v2"));
    releaseOne();
    releaseOne();
    expect(pluginConsumersSnapshot()).toEqual([consumer("v2")]);
  });

  it("keeps two views of one plugin separate", () => {
    registerPluginConsumer(consumer("v1"));
    registerPluginConsumer(consumer("v2"));
    expect(pluginConsumersSnapshot()).toHaveLength(2);
  });

  it("keeps the same view name apart across plugins and projects", () => {
    registerPluginConsumer(consumer("main", "board", "/p/a"));
    registerPluginConsumer(consumer("main", "notes", "/p/a"));
    registerPluginConsumer(consumer("main", "board", "/p/b"));
    expect(pluginConsumersSnapshot()).toHaveLength(3);
  });

  it("does not notify when a no-op release happens", () => {
    let notified = 0;
    const stop = subscribePluginConsumers(() => {
      notified += 1;
    });
    registerPluginConsumer(consumer("v1"));
    const before = notified;
    // Never registered: releasing it must not look like a change.
    consumer("v9");
    stop();
    expect(notified).toBe(before);
  });
});
