import { expect, test } from "vitest";

import { base64urlToBytes } from "./base64url";

/** `bytes` as base64url, through a binary string, the RFC 4648 alphabet swapped in. */
function toBase64url(bytes: readonly number[]): string {
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
}

test("the RFC 4648 vectors decode to their bytes", () => {
  const vectors: readonly (readonly [string, string])[] = [
    ["", ""],
    ["Zg", "f"],
    ["Zm8", "fo"],
    ["Zm9v", "foo"],
    ["Zm9vYg", "foob"],
    ["Zm9vYmE", "fooba"],
    ["Zm9vYmFy", "foobar"],
  ];
  for (const [encoded, text] of vectors) {
    expect([...base64urlToBytes(encoded)]).toEqual([...new TextEncoder().encode(text)]);
  }
});

test("the url alphabet and missing padding decode", () => {
  expect([...base64urlToBytes("-_8")]).toEqual([0xfb, 0xff]);
  expect([...base64urlToBytes("Zm9vYmE")]).toEqual([...new TextEncoder().encode("fooba")]);
});

test("a 65-byte P-256 public key round trips", () => {
  const key = [0x04, ...Array.from({ length: 64 }, (_value, index) => index)];
  const decoded = base64urlToBytes(toBase64url(key));
  expect(decoded.length).toBe(65);
  expect(decoded[0]).toBe(0x04);
  expect([...decoded]).toEqual(key);
});

test("a character outside the alphabet throws", () => {
  expect(() => base64urlToBytes("Zm9v*")).toThrow();
});
