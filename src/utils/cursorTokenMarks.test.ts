import assert from "node:assert/strict";
import test from "node:test";

import { cursorTokenMarkPassed } from "./cursorTokenMarks.ts";

test("only an explicit pass turns a token mark green", () => {
  assert.equal(cursorTokenMarkPassed(true), true);
  assert.equal(cursorTokenMarkPassed(false), false);
  assert.equal(cursorTokenMarkPassed(undefined), false);
  assert.equal(cursorTokenMarkPassed(null), false);
});
