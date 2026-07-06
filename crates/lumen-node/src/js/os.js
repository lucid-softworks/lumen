// node:os over the __os native op. Facts are snapshotted at first access, like Node's.

const __osInfo = __os.info();
const EOL = __osInfo.platform === "win32" ? "\r\n" : "\n";

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
