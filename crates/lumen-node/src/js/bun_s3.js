// S3-compatible object storage over fetch with AWS Signature Version 4 authentication.
{
  const crypto = __builtins.get("crypto");

  function hexHash(value) { return crypto.createHash("sha256").update(value).digest("hex"); }
  function hmac(key, value, encoding) { return crypto.createHmac("sha256", key).update(value).digest(encoding); }
  function encode(value) { return encodeURIComponent(String(value)).replace(/[!'()*]/g, char => `%${char.charCodeAt(0).toString(16).toUpperCase()}`); }
  function encodePath(value) { return String(value).split("/").map(encode).join("/"); }
  function timestamp(date) { return date.toISOString().replace(/[:-]|\.\d{3}/g, ""); }

  function credentials(options = {}) {
    const region = options.region || process.env.AWS_REGION || process.env.AWS_DEFAULT_REGION || "us-east-1";
    const accessKeyId = options.accessKeyId || process.env.AWS_ACCESS_KEY_ID;
    const secretAccessKey = options.secretAccessKey || process.env.AWS_SECRET_ACCESS_KEY;
    const sessionToken = options.sessionToken || process.env.AWS_SESSION_TOKEN;
    const bucket = options.bucket || process.env.S3_BUCKET || process.env.AWS_BUCKET;
    if (!accessKeyId || !secretAccessKey) return s3Error("ERR_S3_MISSING_CREDENTIALS", "S3 credentials are required");
    if (!bucket) return s3Error("ERR_S3_INVALID_PATH", "S3 bucket is required");
    let endpoint;
    try { endpoint = new URL(options.endpoint || process.env.S3_ENDPOINT || `https://s3.${region}.amazonaws.com`); }
    catch (_) { return s3Error("ERR_S3_INVALID_ENDPOINT", "Invalid S3 endpoint"); }
    return { region, accessKeyId: String(accessKeyId), secretAccessKey: String(secretAccessKey), sessionToken, bucket: String(bucket), endpoint };
  }

  function s3Error(code, message) { const error = new Error(message); error.code = code; return error; }
  function signingKey(secret, day, region) {
    return hmac(hmac(hmac(hmac(`AWS4${secret}`, day), region), "s3"), "aws4_request");
  }
  function objectUrl(config, key) {
    const url = new URL(config.endpoint.href);
    const base = url.pathname.replace(/\/$/, "");
    url.pathname = `${base}/${encode(config.bucket)}/${encodePath(key)}`;
    return url;
  }
  function canonicalQuery(url) {
    const values = [];
    for (const [key, value] of url.searchParams) values.push([encode(key), encode(value)]);
    values.sort((a, b) => a[0] === b[0] ? (a[1] < b[1] ? -1 : a[1] > b[1] ? 1 : 0) : a[0] < b[0] ? -1 : 1);
    return values.map(([key, value]) => `${key}=${value}`).join("&");
  }
  function sign(config, method, key, options = {}, payload = Buffer.alloc(0), presign = false) {
    const date = options.date instanceof Date ? options.date : new Date();
    const time = timestamp(date), day = time.slice(0, 8), scope = `${day}/${config.region}/s3/aws4_request`;
    const url = objectUrl(config, key), host = url.host;
    for (const [name, value] of Object.entries(options.query || {})) if (value !== undefined) url.searchParams.set(name, String(value));
    const headers = { host };
    if (options.type) headers["content-type"] = String(options.type);
    if (options.acl) headers["x-amz-acl"] = String(options.acl);
    if (config.sessionToken && !presign) headers["x-amz-security-token"] = config.sessionToken;
    const payloadHash = presign ? "UNSIGNED-PAYLOAD" : hexHash(payload);
    if (!presign) { headers["x-amz-content-sha256"] = payloadHash; headers["x-amz-date"] = time; }
    const headerNames = Object.keys(headers).sort();
    const signedHeaders = headerNames.join(";");
    const canonicalHeaders = headerNames.map(name => `${name}:${String(headers[name]).trim().replace(/\s+/g, " ")}\n`).join("");
    if (presign) {
      const expires = options.expiresIn === undefined ? 86400 : Number(options.expiresIn);
      if (!Number.isInteger(expires) || expires < 1 || expires > 604800) throw s3Error("ERR_S3_INVALID_SIGNATURE", "S3 presign expiry must be between 1 and 604800 seconds");
      url.searchParams.set("X-Amz-Algorithm", "AWS4-HMAC-SHA256");
      url.searchParams.set("X-Amz-Credential", `${config.accessKeyId}/${scope}`);
      url.searchParams.set("X-Amz-Date", time);
      url.searchParams.set("X-Amz-Expires", String(expires));
      url.searchParams.set("X-Amz-SignedHeaders", signedHeaders);
      if (config.sessionToken) url.searchParams.set("X-Amz-Security-Token", config.sessionToken);
      if (options.type) url.searchParams.set("response-content-type", String(options.type));
      if (options.contentDisposition) url.searchParams.set("response-content-disposition", String(options.contentDisposition));
    }
    const canonical = `${method}\n${url.pathname}\n${canonicalQuery(url)}\n${canonicalHeaders}\n${signedHeaders}\n${payloadHash}`;
    const stringToSign = `AWS4-HMAC-SHA256\n${time}\n${scope}\n${hexHash(canonical)}`;
    const signature = hmac(signingKey(config.secretAccessKey, day, config.region), stringToSign, "hex");
    if (presign) url.searchParams.set("X-Amz-Signature", signature);
    else headers.authorization = `AWS4-HMAC-SHA256 Credential=${config.accessKeyId}/${scope}, SignedHeaders=${signedHeaders}, Signature=${signature}`;
    delete headers.host;
    return { url: url.href.replace(/\+/g, "%20"), headers };
  }

  async function bodyBytes(data) {
    if (data instanceof Response) return Buffer.from(await data.arrayBuffer());
    if (typeof Blob !== "undefined" && data instanceof Blob) return Buffer.from(await data.arrayBuffer());
    if (data instanceof ArrayBuffer) return Buffer.from(data);
    if (ArrayBuffer.isView(data)) return Buffer.from(data.buffer, data.byteOffset, data.byteLength);
    return Buffer.from(String(data));
  }

  function xmlDecode(value) {
    return value.replace(/&(?:lt|gt|amp|quot|apos);/g, entity => ({ "&lt;": "<", "&gt;": ">", "&amp;": "&", "&quot;": '"', "&apos;": "'" })[entity]);
  }
  function parseXml(source) {
    const root = { name: "", text: "", children: [] }, stack = [root];
    let offset = 0;
    while (offset < source.length) {
      const open = source.indexOf("<", offset);
      if (open < 0) { stack[stack.length - 1].text += xmlDecode(source.slice(offset)); break; }
      if (open > offset) stack[stack.length - 1].text += xmlDecode(source.slice(offset, open));
      if (source.startsWith("<!--", open)) { const end = source.indexOf("-->", open + 4); if (end < 0) throw new Error("Invalid S3 XML comment"); offset = end + 3; continue; }
      const close = source.indexOf(">", open + 1);
      if (close < 0) throw new Error("Invalid S3 XML response");
      const token = source.slice(open + 1, close).trim();
      if (token[0] === "?" || token[0] === "!") { offset = close + 1; continue; }
      if (token[0] === "/") stack.pop();
      else {
        const selfClosing = token.endsWith("/"), name = token.replace(/\/$/, "").split(/\s+/, 1)[0];
        const node = { name, text: "", children: [] };
        stack[stack.length - 1].children.push(node);
        if (!selfClosing) stack.push(node);
      }
      offset = close + 1;
    }
    return root.children[0] || root;
  }
  function xmlChildren(node, name) { return node.children.filter(child => child.name === name); }
  function xmlText(node, name) { const child = node.children.find(value => value.name === name); return child ? child.text.trim() : undefined; }

  class S3File extends Blob {
    constructor(client, key, options = {}) { super([], options); this.client = client; this.key = String(key); }
    get name() { return this.key; }
    async _response() { return this.client._request("GET", this.key); }
    async arrayBuffer() { return (await this._response()).arrayBuffer(); }
    async bytes() { return new Uint8Array(await this.arrayBuffer()); }
    async text() { return (await this._response()).text(); }
    async json() { return JSON.parse(await this.text()); }
    stream() { return new ReadableStream({ start: controller => this.bytes().then(bytes => { controller.enqueue(bytes); controller.close(); }, error => controller.error(error)) }); }
    write(data, options) { return this.client.write(this.key, data, options); }
    delete(options) { return this.client.delete(this.key, options); }
    unlink(options) { return this.delete(options); }
    exists(options) { return this.client.exists(this.key, options); }
    stat(options) { return this.client.stat(this.key, options); }
    presign(options) { return this.client.presign(this.key, options); }
    get size() { return NaN; }
    get [Symbol.toStringTag]() { return "S3File"; }
  }

  class S3Client {
    constructor(options = {}) { const value = credentials(options); if (value instanceof Error) throw value; this.options = { ...options }; this.config = value; }
    file(key, options) { return new S3File(this, key, options); }
    presign(key, options = {}) { return sign(this.config, String(options.method || "GET").toUpperCase(), key, options, Buffer.alloc(0), true).url; }
    async _request(method, key, data, options = {}) {
      const payload = data === undefined ? Buffer.alloc(0) : await bodyBytes(data);
      const signed = sign(this.config, method, key, options, payload, false);
      const response = await fetch(signed.url, { method, headers: signed.headers, body: method === "GET" || method === "HEAD" ? undefined : payload });
      if (!response.ok && response.status !== 404) { const error = new Error(`S3 request failed with status ${response.status}`); error.name = "S3Error"; error.status = response.status; throw error; }
      return response;
    }
    async write(key, data, options = {}) { const bytes = await bodyBytes(data); const response = await this._request("PUT", key, bytes, options); if (!response.ok) throw new Error(`S3 write failed (${response.status})`); return bytes.length; }
    async delete(key, options = {}) { await this._request("DELETE", key, undefined, options); }
    unlink(key, options) { return this.delete(key, options); }
    async exists(key, options = {}) { return (await this._request("HEAD", key, undefined, options)).status !== 404; }
    async stat(key, options = {}) { const response = await this._request("HEAD", key, undefined, options); if (response.status === 404) return null; return { etag: response.headers.get("etag"), lastModified: new Date(response.headers.get("last-modified")), size: Number(response.headers.get("content-length") || 0), type: response.headers.get("content-type") || "application/octet-stream" }; }
    async list(options = {}) {
      const query = { "list-type": 2, prefix: options.prefix, delimiter: options.delimiter, "max-keys": options.maxKeys, "start-after": options.startAfter, "continuation-token": options.continuationToken, "fetch-owner": options.fetchOwner ? "true" : undefined };
      const response = await this._request("GET", "", undefined, { ...options, query });
      const root = parseXml(await response.text());
      const contents = xmlChildren(root, "Contents").map(node => {
        const ownerNode = xmlChildren(node, "Owner")[0];
        return { key: xmlText(node, "Key"), lastModified: new Date(xmlText(node, "LastModified")), etag: xmlText(node, "ETag"), size: Number(xmlText(node, "Size") || 0), storageClass: xmlText(node, "StorageClass"), owner: ownerNode ? { id: xmlText(ownerNode, "ID"), displayName: xmlText(ownerNode, "DisplayName") } : undefined };
      });
      return { name: xmlText(root, "Name"), prefix: xmlText(root, "Prefix") || "", delimiter: xmlText(root, "Delimiter"), maxKeys: Number(xmlText(root, "MaxKeys") || 0), keyCount: Number(xmlText(root, "KeyCount") || contents.length), isTruncated: xmlText(root, "IsTruncated") === "true", continuationToken: xmlText(root, "ContinuationToken"), nextContinuationToken: xmlText(root, "NextContinuationToken"), startAfter: xmlText(root, "StartAfter"), contents, commonPrefixes: xmlChildren(root, "CommonPrefixes").map(node => ({ prefix: xmlText(node, "Prefix") })) };
    }
  }
  S3Client.list = function (options = {}, config = {}) { return new S3Client(config).list(options); };
  for (const name of ["write", "delete", "unlink", "exists", "stat", "presign"]) {
    S3Client[name] = function (key, data, options) {
      if (name === "write") return new S3Client(options || {}).write(key, data, options || {});
      const config = data || {};
      return new S3Client(config)[name](key, config);
    };
  }

  Object.defineProperty(globalThis, "__lumenS3", { value: { S3Client, S3File }, configurable: true });
}
