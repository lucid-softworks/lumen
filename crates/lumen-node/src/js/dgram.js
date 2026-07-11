// ---- node:dgram -------------------------------------------------------------------------------
// UDP needs raw datagram sockets that lumen does not expose to JS (same story as node:net's
// Socket/Server). The exported surface matches Node's key list, but every constructor/factory
// throws clearly rather than handing back a socket that cannot send or receive.
{
  const notImpl = () => {
    throw new Error("node:dgram sockets are not supported in lumen");
  };
  __builtins.set("dgram", {
    Socket: notImpl,
    createSocket: notImpl,
    _createSocketHandle: notImpl,
  });
}
