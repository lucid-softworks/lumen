// A minimal Node native addon (N-API), written against the stable C ABI. It exports two
// functions — `hello()` and `add(a, b)` — plus a `version` string. lumen loads this exactly as
// Node does: `require('./addon.node')` dlopens the compiled library and calls
// `napi_register_module_v1`, which resolves the `napi_*` symbols against the lumen executable.
//
// The N-API types and functions are declared inline so the addon needs no Node headers — it
// depends only on the ABI, which is what makes a compiled addon portable across engines.

#include <stddef.h>

typedef void *napi_env;
typedef void *napi_value;
typedef void *napi_callback_info;
typedef int napi_status;
typedef napi_value (*napi_callback)(napi_env, napi_callback_info);

extern napi_status napi_create_string_utf8(napi_env, const char *, size_t, napi_value *);
extern napi_status napi_create_double(napi_env, double, napi_value *);
extern napi_status napi_create_function(napi_env, const char *, size_t, napi_callback, void *,
                                        napi_value *);
extern napi_status napi_set_named_property(napi_env, napi_value, const char *, napi_value);
extern napi_status napi_get_cb_info(napi_env, napi_callback_info, size_t *, napi_value *,
                                    napi_value *, void **);
extern napi_status napi_get_value_double(napi_env, napi_value, double *);
extern napi_status napi_throw_error(napi_env, const char *, const char *);

#define NAPI_AUTO_LENGTH ((size_t)-1)

static napi_value hello(napi_env env, napi_callback_info info) {
  napi_value s;
  napi_create_string_utf8(env, "world from native code", NAPI_AUTO_LENGTH, &s);
  return s;
}

static napi_value add(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value argv[2];
  napi_get_cb_info(env, info, &argc, argv, NULL, NULL);
  if (argc < 2) {
    napi_throw_error(env, NULL, "add(a, b) expects two numbers");
    return NULL;
  }
  double a = 0, b = 0;
  napi_get_value_double(env, argv[0], &a);
  napi_get_value_double(env, argv[1], &b);
  napi_value result;
  napi_create_double(env, a + b, &result);
  return result;
}

napi_value napi_register_module_v1(napi_env env, napi_value exports) {
  napi_value fn, version;

  napi_create_function(env, "hello", NAPI_AUTO_LENGTH, hello, NULL, &fn);
  napi_set_named_property(env, exports, "hello", fn);

  napi_create_function(env, "add", NAPI_AUTO_LENGTH, add, NULL, &fn);
  napi_set_named_property(env, exports, "add", fn);

  napi_create_string_utf8(env, "1.0.0", NAPI_AUTO_LENGTH, &version);
  napi_set_named_property(env, exports, "version", version);

  return exports;
}
