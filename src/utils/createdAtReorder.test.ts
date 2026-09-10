import assert from "node:assert/strict";
import test from "node:test";

import {
  applyPageReorder,
  computeCreatedAtUpdates,
} from "./createdAtReorder.ts";

test("desc midpoint sits strictly between neighbors", () => {
  const updates = computeCreatedAtUpdates({
    ordered: [
      { id: "a", created_at: 100 },
      { id: "c", created_at: 70 },
      { id: "b", created_at: 90 },
    ],
    movedId: "c",
    direction: "desc",
    now: 1_000,
  });
  assert.deepEqual(updates, [{ id: "c", created_at: 95 }]);
});

test("desc top uses now or neighbor plus one", () => {
  const updates = computeCreatedAtUpdates({
    ordered: [
      { id: "c", created_at: 70 },
      { id: "a", created_at: 100 },
      { id: "b", created_at: 90 },
    ],
    movedId: "c",
    direction: "desc",
    now: 1_000,
  });
  assert.deepEqual(updates, [{ id: "c", created_at: 1_000 }]);
});

test("desc bottom is one less than previous neighbor", () => {
  const updates = computeCreatedAtUpdates({
    ordered: [
      { id: "b", created_at: 90 },
      { id: "c", created_at: 80 },
      { id: "a", created_at: 100 },
    ],
    movedId: "a",
    direction: "desc",
    now: 1_000,
  });
  assert.deepEqual(updates, [{ id: "a", created_at: 79 }]);
});

test("desc collision bumps the prefix to open an integer gap", () => {
  const updates = computeCreatedAtUpdates({
    ordered: [
      { id: "a", created_at: 10 },
      { id: "c", created_at: 8 },
      { id: "b", created_at: 10 },
    ],
    movedId: "c",
    direction: "desc",
    now: 1_000,
  });
  const byId = Object.fromEntries(
    updates.map((item) => [item.id, item.created_at]),
  );
  assert.equal(byId.a, 12);
  assert.equal(byId.c, 11);
  assert.equal(byId.b, undefined);
});

test("asc midpoint sits strictly between neighbors", () => {
  const updates = computeCreatedAtUpdates({
    ordered: [
      { id: "a", created_at: 10 },
      { id: "c", created_at: 40 },
      { id: "b", created_at: 20 },
    ],
    movedId: "c",
    direction: "asc",
    now: 1_000,
  });
  assert.deepEqual(updates, [{ id: "c", created_at: 15 }]);
});

test("asc bottom uses now or neighbor plus one", () => {
  const updates = computeCreatedAtUpdates({
    ordered: [
      { id: "a", created_at: 10 },
      { id: "b", created_at: 20 },
      { id: "c", created_at: 5 },
    ],
    movedId: "c",
    direction: "asc",
    now: 1_000,
  });
  assert.deepEqual(updates, [{ id: "c", created_at: 1_000 }]);
});

test("applyPageReorder replaces the current page slice", () => {
  assert.deepEqual(
    applyPageReorder(["a", "b", "c", "d"], ["b", "c"], ["c", "b"]),
    ["a", "c", "b", "d"],
  );
});
