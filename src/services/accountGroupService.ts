/**
 * 账号分组服务
 * 数据通过 Tauri 命令持久化到磁盘 (~/.aitool_cockpit/account_groups.json)
 * 内存中维护一份缓存避免频繁 IO
 */

import { invoke } from '@tauri-apps/api/core'

const LEGACY_STORAGE_KEY = 'agtools.account_groups';

let idCounter = 0;
function generateId(): string {
  return `grp_${Date.now()}_${++idCounter}`;
}

export interface AccountGroup {
  id: string;
  name: string;
  accountIds: string[];
  createdAt: number;
}

// ─── 内存缓存 ───────────────────────────────────────
let cachedGroups: AccountGroup[] | null = null;

function cloneGroups(groups: AccountGroup[]): AccountGroup[] {
  return groups.map((group) => ({
    ...group,
    accountIds: [...group.accountIds],
  }));
}

async function loadGroupsFromDisk(): Promise<AccountGroup[]> {
  try {
    const raw: string = await invoke('load_account_groups');
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? cloneGroups(parsed) : [];
  } catch {
    return [];
  }
}

async function saveGroupsToDisk(groups: AccountGroup[]): Promise<void> {
  try {
    await invoke('save_account_groups', { data: JSON.stringify(groups, null, 2) });
  } catch (e) {
    console.error('[AccountGroups] Failed to save to disk:', e);
    throw e;
  }
}

/** 迁移 localStorage 数据到磁盘（仅首次） */
async function migrateLegacyData(): Promise<void> {
  try {
    const raw = localStorage.getItem(LEGACY_STORAGE_KEY);
    if (!raw) return;
    const legacy = JSON.parse(raw);
    if (!Array.isArray(legacy) || legacy.length === 0) return;

    // 检查磁盘上是否已有数据
    const diskData = await loadGroupsFromDisk();
    if (diskData.length > 0) {
      // 磁盘已有数据，清理 localStorage
      localStorage.removeItem(LEGACY_STORAGE_KEY);
      return;
    }

    // 迁移到磁盘
    await saveGroupsToDisk(legacy);
    localStorage.removeItem(LEGACY_STORAGE_KEY);
    console.log('[AccountGroups] Migrated', legacy.length, 'groups from localStorage to disk');
  } catch {
    // ignore migration errors
  }
}

async function loadGroups(): Promise<AccountGroup[]> {
  if (cachedGroups !== null) return cloneGroups(cachedGroups);

  // 首次加载时尝试迁移 localStorage 数据
  await migrateLegacyData();

  cachedGroups = await loadGroupsFromDisk();
  return cloneGroups(cachedGroups);
}

async function saveGroups(groups: AccountGroup[]): Promise<void> {
  const nextGroups = cloneGroups(groups);
  await saveGroupsToDisk(nextGroups);
  cachedGroups = nextGroups;
}

// ─── 公开 API（全部改为 async） ───────────────────────

export async function getAccountGroups(): Promise<AccountGroup[]> {
  return loadGroups();
}

export async function createGroup(name: string): Promise<AccountGroup> {
  const groups = await loadGroups();
  const group: AccountGroup = {
    id: generateId(),
    name: name.trim(),
    accountIds: [],
    createdAt: Date.now(),
  };
  groups.push(group);
  await saveGroups(groups);
  return group;
}

export async function deleteGroup(groupId: string): Promise<void> {
  const groups = (await loadGroups()).filter((g) => g.id !== groupId);
  await saveGroups(groups);
}

export async function renameGroup(groupId: string, name: string): Promise<AccountGroup | null> {
  const groups = await loadGroups();
  const group = groups.find((g) => g.id === groupId);
  if (!group) return null;
  group.name = name.trim();
  await saveGroups(groups);
  return group;
}

export async function addAccountsToGroup(groupId: string, accountIds: string[]): Promise<AccountGroup | null> {
  return assignAccountsToGroup(groupId, accountIds)
}

export async function assignAccountsToGroup(groupId: string, accountIds: string[]): Promise<AccountGroup | null> {
  const groups = await loadGroups();
  const group = groups.find((g) => g.id === groupId);
  if (!group) return null;
  const targetIds = new Set(accountIds);

  for (const currentGroup of groups) {
    if (currentGroup.id === groupId) continue;
    currentGroup.accountIds = currentGroup.accountIds.filter((id) => !targetIds.has(id));
  }

  const existing = new Set(group.accountIds);
  for (const id of accountIds) {
    if (!existing.has(id)) {
      group.accountIds.push(id);
      existing.add(id);
    }
  }
  await saveGroups(groups);
  return group;
}

export async function removeAccountsFromGroup(groupId: string, accountIds: string[]): Promise<AccountGroup | null> {
  const groups = await loadGroups();
  const group = groups.find((g) => g.id === groupId);
  if (!group) return null;
  const toRemove = new Set(accountIds);
  group.accountIds = group.accountIds.filter((id) => !toRemove.has(id));
  await saveGroups(groups);
  return group;
}

/** 清理不存在的账号ID（当账号被删除时调用） */
export async function cleanupDeletedAccounts(existingAccountIds: Set<string>): Promise<void> {
  const groups = await loadGroups();
  let changed = false;
  for (const group of groups) {
    const before = group.accountIds.length;
    group.accountIds = group.accountIds.filter((id) => existingAccountIds.has(id));
    if (group.accountIds.length !== before) changed = true;
  }
  if (changed) await saveGroups(groups);
}

/** 将账号从一个分组移动到另一个分组 */
export async function moveAccountsBetweenGroups(
  fromGroupId: string,
  toGroupId: string,
  accountIds: string[]
): Promise<void> {
  if (fromGroupId === toGroupId) return;
  await assignAccountsToGroup(toGroupId, accountIds);
}

/** 使缓存失效，下次 getAccountGroups 时重新从磁盘读取 */
export function invalidateCache(): void {
  cachedGroups = null;
}
