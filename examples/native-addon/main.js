// Load a compiled native addon the same way Node does: require('./addon.node') dlopens the
// library and runs its N-API registration. Build it first with ./build.sh.
const addon = require("./addon.node");

console.log("addon.version   =>", addon.version);
console.log("addon.hello()   =>", addon.hello());
console.log("addon.add(2, 3) =>", addon.add(2, 3));
console.log("typeof add      =>", typeof addon.add);

try {
  addon.add(1);
} catch (e) {
  console.log("addon.add(1)    => threw:", e.message);
}
