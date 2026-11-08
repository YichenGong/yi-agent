import { describe, expect, it, vi } from "vitest";
import {
  notifyRuntimeSettingsInvalidated,
  onRuntimeSettingsInvalidated,
  resetRuntimeSettingsBus,
} from "./runtimeSettingsBus";

// The bus keys on client *identity*, so a plain object stands in for an RpcClient.
const clientA = {} as never;
const clientB = {} as never;

describe("runtimeSettingsBus", () => {
  it("delivers an invalidation only to the client it belongs to", () => {
    resetRuntimeSettingsBus();
    const listenerA = vi.fn();
    const listenerB = vi.fn();
    onRuntimeSettingsInvalidated(clientA, listenerA);
    onRuntimeSettingsInvalidated(clientB, listenerB);

    notifyRuntimeSettingsInvalidated(clientA);

    expect(listenerA).toHaveBeenCalledTimes(1);
    // A remote project's save must not refresh another host's settings page.
    expect(listenerB).not.toHaveBeenCalled();
  });

  it("stops delivering after unsubscribe", () => {
    resetRuntimeSettingsBus();
    const listener = vi.fn();
    const unsubscribe = onRuntimeSettingsInvalidated(clientA, listener);
    unsubscribe();
    notifyRuntimeSettingsInvalidated(clientA);
    expect(listener).not.toHaveBeenCalled();
  });

  it("keeps notifying the other subscribers when one throws", () => {
    resetRuntimeSettingsBus();
    const failing = vi.fn(() => {
      throw new Error("boom");
    });
    const healthy = vi.fn();
    onRuntimeSettingsInvalidated(clientA, failing);
    onRuntimeSettingsInvalidated(clientA, healthy);
    notifyRuntimeSettingsInvalidated(clientA);
    expect(healthy).toHaveBeenCalledTimes(1);
  });

  it("forgets a client once its last subscriber leaves", () => {
    resetRuntimeSettingsBus();
    const first = vi.fn();
    const unsubscribe = onRuntimeSettingsInvalidated(clientA, first);
    unsubscribe();
    // Registering a new subscriber for the same client must still work; a leaked
    // empty set is harmless but a *stale* one would deliver to dead listeners.
    const second = vi.fn();
    onRuntimeSettingsInvalidated(clientA, second);
    notifyRuntimeSettingsInvalidated(clientA);
    expect(first).not.toHaveBeenCalled();
    expect(second).toHaveBeenCalledTimes(1);
  });
});
