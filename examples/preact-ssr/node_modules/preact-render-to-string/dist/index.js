const mod = require('./commonjs');
mod.default.renderToStringAsync = mod.renderToStringAsync;
mod.default.renderToStaticMarkup = mod.default;
mod.default.renderToString = mod.default;
mod.default.render = mod.default;
module.exports = mod.default;