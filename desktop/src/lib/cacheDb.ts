/**
 * IndexedDB 的身份常量。
 *
 * 单独成文件是为了打断 `contentStore.ts` ↔ `cacheScope.ts` 的循环依赖：
 * 前者需要后者的 `ensureCacheScopeClean`，后者需要前者的库名/表名。
 * 本文件**不得 import 任何东西**。
 */

/** 库名。与 v1 保持一致，本期不迁移、不改名。 */
export const DB_NAME = "yi-agent-cache";

/** 结构版本。本期所有改动都不提升它。 */
export const DB_VERSION = 1;

/** 内容缓存所在的 object store。 */
export const STORE_NAME = "threadContent";
