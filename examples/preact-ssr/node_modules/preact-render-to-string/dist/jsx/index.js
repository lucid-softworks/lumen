const entry = require('./commonjs');
entry.default.render = entry.render;
entry.default.shallowRender = entry.shallowRender;
module.exports = entry.default;