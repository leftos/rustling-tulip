/**
 * Decodes a base64url text, the way a VAPID public key arrives: the `-`/`_`
 * alphabet, padding optional. A character outside the alphabet makes `atob`
 * throw, so a malformed key reaches the panel as an error instead of bytes.
 */
export function base64urlToBytes(text: string): Uint8Array<ArrayBuffer> {
  const standard = text.replaceAll("-", "+").replaceAll("_", "/");
  const padded = standard.padEnd(Math.ceil(standard.length / 4) * 4, "=");
  const binary = atob(padded);
  const bytes = new Uint8Array(new ArrayBuffer(binary.length));
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}
