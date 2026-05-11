/**
 * Module augmentation for WebdriverIO capabilities.
 *
 * `tauri-driver` accepts a non-standard `"tauri:options"` capability that is
 * not modelled by `@wdio/types`. Augmenting `WebdriverIO.Capabilities`
 * (declared as an empty interface in @wdio/types so users can extend it)
 * teaches TypeScript about it without resorting to `any`.
 */
declare global {
  namespace WebdriverIO {
    interface Capabilities {
      "tauri:options"?: {
        application: string;
        args?: string[];
        env?: Record<string, string>;
        webviewOptions?: Record<string, unknown>;
      };
    }
  }
}
export {};
