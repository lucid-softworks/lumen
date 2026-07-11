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
  // getpriority(2) isn't reachable from std; 0 is the default every un-reniced process has.
  getPriority: () => 0,
  setPriority: () => { throw new Error("os.setPriority is not supported in lumen"); },
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
