//! node:crypto asymmetric-half tests (Ed25519/X25519, key plumbing, RSA, ECDSA/ECDH, DH, primes,
//! X.509). Every fixed vector here was produced by Node v22.16.0 / OpenSSL — the tests prove
//! lumen's pure-JS BigInt implementation is interoperable in both directions: Node-made keys and
//! signatures load/verify in lumen, and (in the dev loop, checked live) lumen-made artifacts load
//! in Node. Kept separate from `tests.rs` so parallel work on the symmetric half doesn't collide.

use super::*;
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .expect("utf8 console output")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn test_runtime() -> (Runtime, Captured) {
    let mut rt = Runtime::new();
    let out = Captured::default();
    let err = Captured::default();
    rt.engine().ctx().op_state().put(console::ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(err),
    });
    (rt, out)
}

fn eval_ok(rt: &mut Runtime, src: &str) {
    match rt.eval(src).expect("parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

/// SHA-384/SHA-512 digests + HMAC — vectors from Node v22 (`createHash`/`createHmac`).
#[test]
fn node_crypto_sha512_family() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const c = require("node:crypto");
        console.log(c.createHash("sha512").update("abc").digest("hex"));
        console.log(c.createHash("sha384").update("abc").digest("hex"));
        console.log(c.createHash("sha512").update("").digest("hex").slice(0, 32));
        console.log(c.createHmac("sha512", "key").update("The quick brown fox jumps over the lazy dog").digest("hex").slice(0, 32));
        console.log(c.getHashes().join(","));
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
            "cf83e1357eefb8bdf1542850d66d8007",
            "b42af09057bac1e2d41708e48a902e09",
            "md5,sha1,sha256,sha384,sha512",
        ]
    );
}

/// Ed25519 known-answer test: deterministic seed → public key and signature must match Node's
/// output for the same PKCS#8 key (captured from `node -e` with this exact seed).
#[test]
fn node_crypto_ed25519_known_vector() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const c = require("node:crypto");
        const seed = "9d61b19deffebe6c9edcbe1e1e9e1e9e1e9e1e9e1e9e1e9e1e9e1e9e1e9e1e9e";
        const pkcs8 = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), Buffer.from(seed, "hex")]);
        const priv = c.createPrivateKey({ key: pkcs8, format: "der", type: "pkcs8" });
        const pub = c.createPublicKey(priv);
        const spki = pub.export({ type: "spki", format: "der" });
        console.log(spki.subarray(12).toString("hex"));
        const msg = Buffer.from("726663383033322074657374", "hex");
        const sig = c.sign(null, msg, priv);
        console.log(sig.toString("hex"));
        console.log(c.verify(null, msg, pub, sig));
        console.log(c.verify(null, Buffer.from("other"), pub, sig));
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "09447736ddf5aa8f7ac70c09f9886547ea1122c61be99c2ed62a6ebb9e5e7bcd",
            "99b1fa793326fbb5c565d36d556bc8164a6d2fb64977c0837195dc1aa4693b8ef038429322c94d0e861dd8f90c12ac9819143267e24aadc4d096084184eec203",
            "true",
            "false",
        ]
    );
}

/// Ed25519 generate → export (pem/der/jwk) → import round-trips; sign/verify with the
/// re-imported keys; async callback form of crypto.sign/verify.
#[test]
fn node_crypto_ed25519_roundtrip_and_async() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const c = require("node:crypto");
        const { publicKey, privateKey } = c.generateKeyPairSync("ed25519");
        console.log(publicKey.type, privateKey.type, publicKey.asymmetricKeyType);
        const pubPem = publicKey.export({ type: "spki", format: "pem" });
        const privPem = privateKey.export({ type: "pkcs8", format: "pem" });
        const pub2 = c.createPublicKey(pubPem);
        const priv2 = c.createPrivateKey(privPem);
        console.log(pub2.equals(publicKey), priv2.equals(privateKey));
        const jwk = privateKey.export({ format: "jwk" });
        console.log(jwk.kty, jwk.crv, c.createPrivateKey({ key: jwk, format: "jwk" }).equals(privateKey));
        console.log(c.createPublicKey(privateKey).equals(publicKey));
        const msg = Buffer.from("round trip");
        const sig = c.sign(null, msg, priv2);
        console.log(c.verify(null, msg, pub2, sig));
        c.sign(null, msg, privPem, (err, s) => {
          c.verify(null, msg, pubPem, s, (err2, ok) => console.log("async", ok));
        });
        const { publicKey: p2 } = c.generateKeyPairSync("ed25519");
        console.log("cross", c.verify(null, msg, p2, sig));
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "public private ed25519",
            "true true",
            "OKP Ed25519 true",
            "true",
            "true",
            "cross false",
            "async true",
        ]
    );
}

/// Node-generated RSA-2048, EC P-256 and X25519 keys (static fixtures) import in every format;
/// cross-format identity holds; details are reported like Node.
#[test]
fn node_crypto_key_import_node_fixtures() {
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        r#"
        const c = require("node:crypto");
        const rsaPkcs8 = `-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQClvFJpLQV/TVM/
of4KAF6lfMfQZ6A62aJVJUfQdDi96nqQK7Fv23GRLcg1ALWiIup4rA9UhKYOqY/m
wQbjwQF36ZcQ0kEFDVKfygQRD5vkLR1Zhoafb/7VY+IfUetLMhhYcvSVt2/fiOZZ
IJ8+Jd7tr+EvyoZs1Bap8ZYGASuCgpIMEl+m0r14rA1idqskqyZpm17FzS3bXRcv
uWSQtNI+opT6n1N0mRUlaPwhU/tSbpTkxV5L7uNeupMIU81UOjyjim0soJq7NK/j
oDgiP5LS+48GfPlgud3+DBZeMEsuYmhm6T4FGX48k060ivl3jwwzTeOKLnXNgvit
X7MmACVXAgMBAAECggEADO+/sQT6Hl8vRdLDrQ0tVhaA1EQabi2JrcK8sck4mp5g
VbuLXJNQ8IeVzolRJCg8jBHGGo9GFPPoTxc3DfUFQ9JgX8hBmf7Zua4/lgNVZECf
P1swS+amigqDXnV6+8Ibw1+ptrv8SAL8E+3ncCbtmTj2x0+0IW+Gm1pHnw1dU5D0
UVnVq2GzQg7JyqgZe7FRSU/8jdcuzj8yA+jSy70zH6Mjf4b/pGz23mmdB16zPocc
Bkkgf6X9eO/OgSKXaGptwQgQUBqGooZZoCbM3J7VOzDsgVGZXNDKcxngNcSTQemK
23ToelrUtJC3vXq2ObjyI56l7J9oU+dMSXplM5phVQKBgQDce3nrrF4ENPMSrHU+
KLvmput1sPGYsZt4GHM7uhOyHPO4kZRraH6xbj1geCPKxlaXacuSrlFTzzOTXH7V
NZy1Z/+yx/uy77FDvSl47BvPpPumoQw4dgwFw/2mLkHTd7z7+aCjt+ak3e3XWJGK
weXd+84aWS3H4pIQ6iZIsJpxbQKBgQDAbyIsTfX69ZoCeHT4ZFx/AnRdZHDbRztw
vrRLyn5zsCuQVc0EwFTGrwOZz2KnPXtf+OD7/pOFWveZwnJuMrDv05WrpZWUfoV9
5Ai/5Buc2EQ7qETbHneInLoS0q9vm2HZxeXxvYHD9MHTbirw08ddLgbHAD5Cztwt
dcWntmZ7UwKBgBciyddmHfN5DuytthvQsG7yoxCVgbSRJoxCnIzu6LQu/5Aljpp6
u5ioxb4CvVbA20NGMbtxmU0fF/1lnlWHK6uJfzZmb84GAublyZ1LwVtXp6SDj8G4
+Wf9efdfMT8ceHNEbYvgd05jj1qii5sw34scqjLvmrM33jXyLBRCm+I9AoGBAK2y
otIC/QmuL3oTaOHdFXC/snGqfAQyZAD84pmXClU6q9f42rpzMRK2XzWy8IWtBXQ3
nj1YKaix19U+ozO9JeEUx4DMUhxbp/tenlc3e4Uz4UNIO/7dnV/+uCbNbfX793Mv
IsP2Hu/WOi6yvqfrQYVmSk/OdGSxfCS8rdEY36BpAoGATNrqzHXJ+T2b37NYBUuW
siqXghCZy2rucwKPRiME9nRS2s+Uqjt5ECYJecksz5J/Zin1yTNkB07Z6GrbNuHA
zAJJ0z5O5k4zNCpehO4mcjotTP9GUi75KBoj+yDZ/xvfw6NXaL2i82g8yzRAysHg
m9MPTXoLuzh+6gYQx+4jir8=
-----END PRIVATE KEY-----`;
        const rsaPub = `-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEApbxSaS0Ff01TP6H+CgBe
pXzH0GegOtmiVSVH0HQ4vep6kCuxb9txkS3INQC1oiLqeKwPVISmDqmP5sEG48EB
d+mXENJBBQ1Sn8oEEQ+b5C0dWYaGn2/+1WPiH1HrSzIYWHL0lbdv34jmWSCfPiXe
7a/hL8qGbNQWqfGWBgErgoKSDBJfptK9eKwNYnarJKsmaZtexc0t210XL7lkkLTS
PqKU+p9TdJkVJWj8IVP7Um6U5MVeS+7jXrqTCFPNVDo8o4ptLKCauzSv46A4Ij+S
0vuPBnz5YLnd/gwWXjBLLmJoZuk+BRl+PJNOtIr5d48MM03jii51zYL4rV+zJgAl
VwIDAQAB
-----END PUBLIC KEY-----`;
        const ecPkcs8 = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgFh42OFDOBgnKrnMx
c5mRpYVGXpfyeFn7SK35cXhNpDyhRANCAAQp2yXEfbGx0yH+NAuOHOT2rfcmwq16
7wuPj5jBVbQa3/JBmQ8Y1IulP7N+x0NhBYOivbM/G0UFrM1PBJ6/vS/K
-----END PRIVATE KEY-----`;
        const ecPub = `-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEKdslxH2xsdMh/jQLjhzk9q33JsKt
eu8Lj4+YwVW0Gt/yQZkPGNSLpT+zfsdDYQWDor2zPxtFBazNTwSev70vyg==
-----END PUBLIC KEY-----`;
        const xPriv = `-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VuBCIEIFDuY7guXClmoWbcwHe6r6GFdWEok1rUENEbL29EsWZf
-----END PRIVATE KEY-----`;
        const xPub = `-----BEGIN PUBLIC KEY-----
MCowBQYDK2VuAyEACwAhLK8q3ay66f9tHbdmuMqRoumN7fcqxx9lglsFfHU=
-----END PUBLIC KEY-----`;

        const rsa = c.createPrivateKey(rsaPkcs8);
        console.log(rsa.asymmetricKeyType, rsa.asymmetricKeyDetails.modulusLength, rsa.asymmetricKeyDetails.publicExponent);
        console.log(c.createPublicKey(rsa).equals(c.createPublicKey(rsaPub)));
        // pkcs1 export → import round trip inside lumen
        const rsa1 = c.createPrivateKey(rsa.export({ type: "pkcs1", format: "pem" }));
        console.log(rsa1.equals(rsa));
        const rsaJwk = c.createPrivateKey({ key: rsa.export({ format: "jwk" }), format: "jwk" });
        console.log(rsaJwk.equals(rsa));

        const ec = c.createPrivateKey(ecPkcs8);
        console.log(ec.asymmetricKeyType, ec.asymmetricKeyDetails.namedCurve);
        console.log(c.createPublicKey(ec).equals(c.createPublicKey(ecPub)));
        const ecSec1 = c.createPrivateKey(ec.export({ type: "sec1", format: "pem" }));
        console.log(ecSec1.equals(ec));
        const ecJwk = c.createPrivateKey({ key: ec.export({ format: "jwk" }), format: "jwk" });
        console.log(ecJwk.equals(ec), ec.export({ format: "jwk" }).crv);

        const x = c.createPrivateKey(xPriv);
        console.log(x.asymmetricKeyType, c.createPublicKey(x).equals(c.createPublicKey(xPub)));
        "#,
    );
    assert_eq!(
        out.lines(),
        [
            "rsa 2048 65537",
            "true",
            "true",
            "true",
            "ec prime256v1",
            "true",
            "true",
            "true P-256",
            "x25519 true",
        ]
    );
}
