// bun:sqlite — Bun's embedded SQLite binding.
//
// SQLite is a C library; Bun links libsqlite3. lumen's workspace is deliberately lean and takes no
// third-party crate, so there is no SQLite engine to drive. We export the real class *shapes* (so
// `import { Database } from "bun:sqlite"` and `instanceof` checks resolve, and tools can feature-
// detect) but every constructor / entry point throws honestly — never a fake in-memory DB. The
// pieces that are pure data are real: `SQLiteError` is a genuine Error subclass, and `constants`
// carries SQLite's actual numeric flag values (copied verbatim from bun v1.2.21).
{
  const NOT_SUPPORTED = "bun:sqlite is not supported in lumen";

  // A real Error subclass — `instanceof Error` and `instanceof SQLiteError` both hold.
  class SQLiteError extends Error {
    constructor(message, code, errno) {
      super(message);
      this.name = "SQLiteError";
      this.code = code;
      this.errno = errno;
    }
  }

  class Statement {
    constructor() {
      throw new SQLiteError(NOT_SUPPORTED);
    }
  }

  class Database {
    constructor() {
      throw new SQLiteError(NOT_SUPPORTED);
    }
    static open() {
      throw new SQLiteError(NOT_SUPPORTED);
    }
    static deserialize() {
      throw new SQLiteError(NOT_SUPPORTED);
    }
    static setCustomSQLite() {
      throw new SQLiteError(NOT_SUPPORTED);
    }
  }
  Database.MAX_QUERY_CACHE_SIZE = 20;

  // SQLite's real flag values (open/prepare/deserialize/fcntl), copied from bun v1.2.21.
  const constants = {
    SQLITE_OPEN_READONLY: 1,
    SQLITE_OPEN_READWRITE: 2,
    SQLITE_OPEN_CREATE: 4,
    SQLITE_OPEN_DELETEONCLOSE: 8,
    SQLITE_OPEN_EXCLUSIVE: 16,
    SQLITE_OPEN_AUTOPROXY: 32,
    SQLITE_OPEN_URI: 64,
    SQLITE_OPEN_MEMORY: 128,
    SQLITE_OPEN_MAIN_DB: 256,
    SQLITE_OPEN_TEMP_DB: 512,
    SQLITE_OPEN_TRANSIENT_DB: 1024,
    SQLITE_OPEN_MAIN_JOURNAL: 2048,
    SQLITE_OPEN_TEMP_JOURNAL: 4096,
    SQLITE_OPEN_SUBJOURNAL: 8192,
    SQLITE_OPEN_SUPER_JOURNAL: 16384,
    SQLITE_OPEN_NOMUTEX: 32768,
    SQLITE_OPEN_FULLMUTEX: 65536,
    SQLITE_OPEN_SHAREDCACHE: 131072,
    SQLITE_OPEN_PRIVATECACHE: 262144,
    SQLITE_OPEN_WAL: 524288,
    SQLITE_OPEN_NOFOLLOW: 16777216,
    SQLITE_OPEN_EXRESCODE: 33554432,
    SQLITE_PREPARE_PERSISTENT: 1,
    SQLITE_PREPARE_NORMALIZE: 2,
    SQLITE_PREPARE_NO_VTAB: 4,
    SQLITE_DESERIALIZE_READONLY: 4,
    SQLITE_FCNTL_LOCKSTATE: 1,
    SQLITE_FCNTL_GET_LOCKPROXYFILE: 2,
    SQLITE_FCNTL_SET_LOCKPROXYFILE: 3,
    SQLITE_FCNTL_LAST_ERRNO: 4,
    SQLITE_FCNTL_SIZE_HINT: 5,
    SQLITE_FCNTL_CHUNK_SIZE: 6,
    SQLITE_FCNTL_FILE_POINTER: 7,
    SQLITE_FCNTL_SYNC_OMITTED: 8,
    SQLITE_FCNTL_WIN32_AV_RETRY: 9,
    SQLITE_FCNTL_PERSIST_WAL: 10,
    SQLITE_FCNTL_OVERWRITE: 11,
    SQLITE_FCNTL_VFSNAME: 12,
    SQLITE_FCNTL_POWERSAFE_OVERWRITE: 13,
    SQLITE_FCNTL_PRAGMA: 14,
    SQLITE_FCNTL_BUSYHANDLER: 15,
    SQLITE_FCNTL_TEMPFILENAME: 16,
    SQLITE_FCNTL_MMAP_SIZE: 18,
    SQLITE_FCNTL_TRACE: 19,
    SQLITE_FCNTL_HAS_MOVED: 20,
    SQLITE_FCNTL_SYNC: 21,
    SQLITE_FCNTL_COMMIT_PHASETWO: 22,
    SQLITE_FCNTL_WIN32_SET_HANDLE: 23,
    SQLITE_FCNTL_WAL_BLOCK: 24,
    SQLITE_FCNTL_ZIPVFS: 25,
    SQLITE_FCNTL_RBU: 26,
    SQLITE_FCNTL_VFS_POINTER: 27,
    SQLITE_FCNTL_JOURNAL_POINTER: 28,
    SQLITE_FCNTL_WIN32_GET_HANDLE: 29,
    SQLITE_FCNTL_PDB: 30,
    SQLITE_FCNTL_BEGIN_ATOMIC_WRITE: 31,
    SQLITE_FCNTL_COMMIT_ATOMIC_WRITE: 32,
    SQLITE_FCNTL_ROLLBACK_ATOMIC_WRITE: 33,
    SQLITE_FCNTL_LOCK_TIMEOUT: 34,
    SQLITE_FCNTL_DATA_VERSION: 35,
    SQLITE_FCNTL_SIZE_LIMIT: 36,
    SQLITE_FCNTL_CKPT_DONE: 37,
    SQLITE_FCNTL_RESERVE_BYTES: 38,
    SQLITE_FCNTL_CKPT_START: 39,
    SQLITE_FCNTL_EXTERNAL_READER: 40,
    SQLITE_FCNTL_CKSM_FILE: 41,
    SQLITE_FCNTL_RESET_CACHE: 42,
  };

  __builtins.set("bun:sqlite", {
    __esModule: true,
    default: Database,
    Database,
    Statement,
    SQLiteError,
    constants,
  });
}
