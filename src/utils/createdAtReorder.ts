export type CreatedAtSortDirection = "asc" | "desc";

export type CreatedAtReorderItem = {
  id: string;
  created_at: number;
};

export type CreatedAtUpdate = {
  id: string;
  created_at: number;
};

function normalizeTs(value: number): number {
  if (!Number.isFinite(value)) return 0;
  return Math.max(0, Math.floor(value));
}

function bumpRange(
  ordered: CreatedAtReorderItem[],
  fromIndex: number,
  toIndex: number,
  delta: number,
) {
  if (delta === 0 || fromIndex > toIndex) return;
  for (let index = fromIndex; index <= toIndex; index += 1) {
    ordered[index].created_at = Math.max(0, ordered[index].created_at + delta);
  }
}

function applyDescOrder(
  ordered: CreatedAtReorderItem[],
  movedIndex: number,
  now: number,
) {
  const prev = movedIndex > 0 ? ordered[movedIndex - 1] : null;
  const next =
    movedIndex < ordered.length - 1 ? ordered[movedIndex + 1] : null;
  const moved = ordered[movedIndex];

  if (!prev && next) {
    moved.created_at = Math.max(now, next.created_at + 1);
    return;
  }

  if (prev && !next) {
    if (prev.created_at <= 0) {
      bumpRange(ordered, 0, movedIndex - 1, 1);
      moved.created_at = 0;
      return;
    }
    moved.created_at = prev.created_at - 1;
    return;
  }

  if (!prev || !next) return;

  if (prev.created_at - next.created_at >= 2) {
    moved.created_at = Math.floor((prev.created_at + next.created_at) / 2);
    return;
  }

  const targetPrev = next.created_at + 2;
  bumpRange(ordered, 0, movedIndex - 1, targetPrev - prev.created_at);
  moved.created_at = next.created_at + 1;
}

function applyAscOrder(
  ordered: CreatedAtReorderItem[],
  movedIndex: number,
  now: number,
) {
  const prev = movedIndex > 0 ? ordered[movedIndex - 1] : null;
  const next =
    movedIndex < ordered.length - 1 ? ordered[movedIndex + 1] : null;
  const moved = ordered[movedIndex];

  if (!prev && next) {
    if (next.created_at <= 0) {
      bumpRange(ordered, movedIndex + 1, ordered.length - 1, 1);
      moved.created_at = 0;
      return;
    }
    moved.created_at = next.created_at - 1;
    return;
  }

  if (prev && !next) {
    moved.created_at = Math.max(now, prev.created_at + 1);
    return;
  }

  if (!prev || !next) return;

  if (next.created_at - prev.created_at >= 2) {
    moved.created_at = Math.floor((prev.created_at + next.created_at) / 2);
    return;
  }

  const targetNext = prev.created_at + 2;
  bumpRange(
    ordered,
    movedIndex + 1,
    ordered.length - 1,
    targetNext - next.created_at,
  );
  moved.created_at = prev.created_at + 1;
}

export function computeCreatedAtUpdates(options: {
  ordered: readonly CreatedAtReorderItem[];
  movedId: string;
  direction: CreatedAtSortDirection;
  now?: number;
}): CreatedAtUpdate[] {
  const now = options.now ?? Math.floor(Date.now() / 1000);
  const ordered = options.ordered.map((item) => ({
    id: item.id,
    created_at: normalizeTs(item.created_at),
  }));
  const movedIndex = ordered.findIndex((item) => item.id === options.movedId);
  if (movedIndex < 0 || ordered.length <= 1) {
    return [];
  }

  if (options.direction === "asc") {
    applyAscOrder(ordered, movedIndex, now);
  } else {
    applyDescOrder(ordered, movedIndex, now);
  }

  const originalById = new Map(
    options.ordered.map((item) => [item.id, normalizeTs(item.created_at)]),
  );
  return ordered
    .filter((item) => originalById.get(item.id) !== item.created_at)
    .map((item) => ({ id: item.id, created_at: item.created_at }));
}

export function applyPageReorder(
  fullOrderedIds: string[],
  originalPageIds: string[],
  nextPageIds: string[],
): string[] {
  if (
    originalPageIds.length === 0 ||
    originalPageIds.length !== nextPageIds.length
  ) {
    return [...fullOrderedIds];
  }
  const start = fullOrderedIds.indexOf(originalPageIds[0]);
  if (start < 0) return [...fullOrderedIds];
  const slice = fullOrderedIds.slice(start, start + originalPageIds.length);
  if (
    slice.length !== originalPageIds.length ||
    slice.some((id, index) => id !== originalPageIds[index])
  ) {
    return [...fullOrderedIds];
  }
  return [
    ...fullOrderedIds.slice(0, start),
    ...nextPageIds,
    ...fullOrderedIds.slice(start + originalPageIds.length),
  ];
}
