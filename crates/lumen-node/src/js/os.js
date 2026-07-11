// node:os over the __os native op. Facts are snapshotted at first access, like Node's.

const __osInfo = __os.info();
const EOL = __osInfo.platform === "win32" ? "\r\n" : "\n";

// libuv errno descriptions for the codes getpriority/setpriority can raise.
const __ERRNO_DESC = { EPERM: "operation not permitted", ESRCH: "no such process", EACCES: "permission denied", EINVAL: "invalid argument", UNKNOWN: "unknown error" };
function systemError(syscall, info) {
  const code = info.code || "UNKNOWN";
  const desc = __ERRNO_DESC[code] || "unknown error";
  const err = new Error(`A system error occurred: ${syscall} returned ${code} (${desc})`);
  err.code = "ERR_SYSTEM_ERROR";
  err.errno = info.errno;
  err.syscall = syscall;
  err.info = { errno: info.errno, code, message: desc, syscall };
  return err;
}
function validatePid(pid, name) {
  if (typeof pid !== "number" || !Number.isInteger(pid)) {
    const e = new TypeError(`The "${name}" argument must be of type number. Received ${typeof pid}`);
    e.code = "ERR_INVALID_ARG_TYPE";
    throw e;
  }
  return pid;
}

const os = {
  EOL,
  platform: () => __osInfo.platform,
  arch: () => __osInfo.arch,
  type: () => __osInfo.type,
  release: () => __osInfo.release,
  version: () => __osInfo.release,
  homedir: () => __osInfo.homedir,
  tmpdir: () => __osInfo.tmpdir,
  hostname: () => __os.hostname(),
  endianness: () => __osInfo.endianness,
  // A minimal cpus() — count is real; per-core model/speed/times aren't reachable from std.
  cpus: () =>
    Array.from({ length: __osInfo.cpus }, () => ({
      model: "unknown",
      speed: 0,
      times: { user: 0, nice: 0, sys: 0, idle: 0, irq: 0 },
    })),
  availableParallelism: () => __osInfo.cpus,
  // uname -m spelling of the arch (Node reports "arm64" on darwin but "aarch64" on linux).
  machine: () =>
    __osInfo.arch === "x64" ? "x86_64"
    : __osInfo.arch === "arm64" ? (__osInfo.platform === "darwin" ? "arm64" : "aarch64")
    : __osInfo.arch === "ia32" ? "i686"
    : __osInfo.arch,
  // Enumerating real interfaces needs getifaddrs(), which std doesn't expose; loopback is the one
  // interface every host has, so report just it (correct, if incomplete) rather than {}.
  networkInterfaces: () => ({
    [__osInfo.platform === "darwin" ? "lo0" : "lo"]: [
      { address: "127.0.0.1", netmask: "255.0.0.0", family: "IPv4", mac: "00:00:00:00:00:00", internal: true, cidr: "127.0.0.1/8" },
      { address: "::1", netmask: "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", family: "IPv6", mac: "00:00:00:00:00:00", internal: true, cidr: "::1/128", scopeid: 0 },
    ],
  }),
  // getPriority/setPriority over getpriority(2)/setpriority(2) (see __os in lib.rs). The native op
  // returns the value/undefined on success, or { errno, code } on failure, which we wrap in Node's
  // ERR_SYSTEM_ERROR exactly as libuv does.
  getPriority: (pid = 0) => {
    const r = __os.getPriority(validatePid(pid, "pid"));
    if (r && typeof r === "object") throw systemError("uv_os_getpriority", r);
    return r;
  },
  setPriority: (...args) => {
    // Node: setPriority(priority) or setPriority(pid, priority).
    let pid = 0, priority;
    if (args.length >= 2) { pid = validatePid(args[0], "pid"); priority = args[1]; }
    else priority = args[0];
    if (typeof priority !== "number" || !Number.isInteger(priority)) {
      const e = new TypeError(`The "priority" argument must be of type number. Received ${typeof priority}`);
      e.code = "ERR_INVALID_ARG_TYPE";
      throw e;
    }
    if (priority < -20 || priority > 19) {
      const e = new RangeError(`The value of "priority" is out of range. It must be >= -20 && <= 19. Received ${priority}`);
      e.code = "ERR_OUT_OF_RANGE";
      throw e;
    }
    const r = __os.setPriority(pid, priority);
    if (r && typeof r === "object") throw systemError("uv_os_setpriority", r);
  },
  totalmem: () => 0,
  freemem: () => 0,
  uptime: () => 0,
  loadavg: () => [0, 0, 0],
  userInfo: () => ({
    username: (__osInfo.homedir.split(/[\\/]/).pop()) || "",
    homedir: __osInfo.homedir,
    shell: null,
    uid: -1,
    gid: -1,
  }),
  constants: { signals: {}, errno: {} },
  devNull: __osInfo.platform === "win32" ? "\\\\.\\nul" : "/dev/null",
};

__builtins.set("os", os);
