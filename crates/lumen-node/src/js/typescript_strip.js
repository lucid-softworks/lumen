// Location-preserving TypeScript erasure for node:module.stripTypeScriptTypes(). This deliberately
// handles only erasable syntax. Constructs that need emitted JavaScript are rejected, as Node's
// strip mode does, and uncertain syntax is rejected instead of being rewritten incorrectly.
{
  const idStart = ch => /[A-Za-z_$]/.test(ch || "");
  const idPart = ch => /[A-Za-z0-9_$]/.test(ch || "");

  function syntaxError(message) {
    const error = new SyntaxError(message);
    error.code = "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX";
    return error;
  }

  function lexicalMask(source) {
    const mask = new Uint8Array(source.length);
    for (let i = 0; i < source.length;) {
      const ch = source[i];
      if (ch === "'" || ch === '"' || ch === "`") {
        const quote = ch;
        mask[i++] = 1;
        while (i < source.length) {
          mask[i] = 1;
          if (source[i] === "\\") {
            if (++i < source.length) mask[i++] = 1;
          } else if (source[i++] === quote) break;
        }
      } else if (ch === "/" && source[i + 1] === "/") {
        mask[i++] = mask[i++] = 1;
        while (i < source.length && source[i] !== "\n") mask[i++] = 1;
      } else if (ch === "/" && source[i + 1] === "*") {
        mask[i++] = mask[i++] = 1;
        while (i < source.length) {
          mask[i] = 1;
          if (source[i] === "*" && source[i + 1] === "/") {
            mask[i + 1] = 1;
            i += 2;
            break;
          }
          i++;
        }
      } else i++;
    }
    return mask;
  }

  function blankRange(chars, start, end) {
    for (let i = start; i < end; i++) if (chars[i] !== "\n" && chars[i] !== "\r") chars[i] = " ";
  }

  function skipSpace(source, i) {
    while (i < source.length && /\s/.test(source[i])) i++;
    return i;
  }

  function wordAt(source, mask, i) {
    if (mask[i] || !idStart(source[i]) || (i > 0 && idPart(source[i - 1]))) return null;
    let end = i + 1;
    while (idPart(source[end])) end++;
    return { value: source.slice(i, end), end };
  }

  function typeEnd(source, mask, start, stops) {
    let angle = 0, square = 0, paren = 0, brace = 0;
    for (let i = start; i < source.length; i++) {
      if (mask[i]) continue;
      const ch = source[i];
      if (!angle && !square && !paren && !brace) {
        if (stops.includes(ch)) return i;
        if (ch === "=" && source[i + 1] === ">" && stops.includes("=>")) return i;
      }
      if (ch === "<") angle++;
      else if (ch === ">" && angle) angle--;
      else if (ch === "[") square++;
      else if (ch === "]" && square) square--;
      else if (ch === "(") paren++;
      else if (ch === ")") { if (paren) paren--; else if (!angle && !square && !brace && stops.includes(")")) return i; }
      else if (ch === "{") brace++;
      else if (ch === "}") { if (brace) brace--; else if (!angle && !square && !paren && stops.includes("}")) return i; }
    }
    return source.length;
  }

  function matching(source, mask, start, open, close) {
    let depth = 0;
    for (let i = start; i < source.length; i++) {
      if (mask[i]) continue;
      if (source[i] === open) depth++;
      else if (source[i] === close && --depth === 0) return i;
    }
    throw new SyntaxError(`Unterminated '${open}' in TypeScript source`);
  }

  function eraseDeclarations(source, chars, mask) {
    for (let i = 0; i < source.length;) {
      const token = wordAt(source, mask, i);
      if (!token) { i++; continue; }
      if (["enum", "namespace"].includes(token.value)) throw syntaxError(`TypeScript ${token.value} declarations require transformation`);
      let start = i;
      let kind = token.value;
      let cursor = token.end;
      if (kind === "export") {
        cursor = skipSpace(source, cursor);
        const next = wordAt(source, mask, cursor);
        if (!next) { i = token.end; continue; }
        kind = next.value;
        cursor = next.end;
      }
      if (kind === "interface") {
        const open = source.indexOf("{", cursor);
        if (open < 0 || mask[open]) throw new SyntaxError("Invalid TypeScript interface declaration");
        const end = matching(source, mask, open, "{", "}") + 1;
        blankRange(chars, start, end);
        i = end;
      } else if (kind === "type") {
        const end = typeEnd(source, mask, cursor, ";\n");
        blankRange(chars, start, end + (source[end] === ";" ? 1 : 0));
        i = end + 1;
      } else if (kind === "import") {
        const after = skipSpace(source, cursor);
        const next = wordAt(source, mask, after);
        if (next && next.value === "type") {
          const end = typeEnd(source, mask, next.end, ";\n");
          blankRange(chars, start, end + (source[end] === ";" ? 1 : 0));
          i = end + 1;
        } else i = token.end;
      } else i = token.end;
    }
  }

  function eraseAnnotation(source, chars, mask, colon, stops) {
    let start = colon;
    if (source[colon - 1] === "?") start--;
    const end = typeEnd(source, mask, colon + 1, stops);
    if (skipSpace(source, colon + 1) === end) throw new SyntaxError("Empty TypeScript type annotation");
    blankRange(chars, start, end);
    return end;
  }

  function eraseVariableAnnotations(source, chars, mask) {
    const pattern = /\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)(\s*\?)?\s*:/g;
    let match;
    while ((match = pattern.exec(source))) {
      const colon = pattern.lastIndex - 1;
      if (mask[match.index] || mask[colon]) continue;
      eraseAnnotation(source, chars, mask, colon, "=,;\n");
    }
  }

  function eraseParameterList(source, chars, mask, open, close) {
    let depth = 0;
    for (let i = open + 1; i < close; i++) {
      if (mask[i]) continue;
      const ch = source[i];
      if ("([{<".includes(ch)) depth++;
      else if (")]}>".includes(ch) && depth) depth--;
      else if (ch === ":" && depth === 0) i = eraseAnnotation(source, chars, mask, i, "=,)") - 1;
    }
  }

  function eraseFunctions(source, chars, mask) {
    for (let i = 0; i < source.length; i++) {
      if (mask[i] || source[i] !== "(") continue;
      const close = matching(source, mask, i, "(", ")");
      let after = skipSpace(source, close + 1);
      let isCallable = source.slice(after, after + 2) === "=>" || source[after] === "{" || source[after] === ":";
      if (!isCallable) continue;
      eraseParameterList(source, chars, mask, i, close);
      if (source[after] === ":") eraseAnnotation(source, chars, mask, after, "{=>");
      i = close;
    }
  }

  function eraseAssertions(source, chars, mask) {
    for (let i = 0; i < source.length;) {
      const token = wordAt(source, mask, i);
      if (!token || (token.value !== "as" && token.value !== "satisfies")) { i += token ? token.value.length : 1; continue; }
      const end = typeEnd(source, mask, token.end, ",;\n)}]");
      blankRange(chars, i, end);
      i = end;
    }
  }

  function stripTypeScriptTypes(code, options = {}) {
    if (typeof code !== "string") throw new TypeError('The "code" argument must be of type string');
    if (options == null || typeof options !== "object") throw new TypeError('The "options" argument must be of type object');
    if (options.mode !== undefined && options.mode !== "strip") throw new TypeError("Only TypeScript strip mode is supported");
    if (options.sourceMap) throw new TypeError("sourceMap is not available in TypeScript strip mode");
    const source = code;
    const chars = source.split("");
    const mask = lexicalMask(source);
    eraseDeclarations(source, chars, mask);
    eraseVariableAnnotations(source, chars, mask);
    eraseFunctions(source, chars, mask);
    eraseAssertions(source, chars, mask);
    let result = chars.join("");
    if (options.sourceUrl !== undefined) result += `\n\n//# sourceURL=${String(options.sourceUrl)};`;
    return result;
  }

  Object.defineProperty(globalThis, "__lumenStripTypeScriptTypes", {
    value: stripTypeScriptTypes, configurable: true,
  });
}
