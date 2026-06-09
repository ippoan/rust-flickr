// rust-flickr REST API の typed fetch クライアント (front 取り込み用)。
// 型は ts-rs が `bindings/` に生成したものを参照する (Rust struct が SoT)。
//
// 使い方 (nuxt 等):
//   const flickr = createFlickrClient({
//     baseUrl: "https://<edge-worker-or-cloud-run>",
//     organizationId: "<organization uuid>",
//   });
//   const { authorization_url } = await flickr.oauthUrl();

import type { ImportRequest } from "../../bindings/ImportRequest";
import type { ImportResponse } from "../../bindings/ImportResponse";
import type { OauthCallbackRequest } from "../../bindings/OauthCallbackRequest";
import type { OauthCallbackResponse } from "../../bindings/OauthCallbackResponse";
import type { OauthUrlResponse } from "../../bindings/OauthUrlResponse";

export interface FlickrClientOptions {
  baseUrl: string;
  /** X-Organization-Id ヘッダ (必須 — 省略時はサーバが 400 を返す) */
  organizationId: string;
  fetchImpl?: typeof fetch;
}

export class FlickrApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly body: { error?: string; message?: string },
  ) {
    super(`rust-flickr API error ${status}: ${body.message ?? "unknown"}`);
    this.name = "FlickrApiError";
  }
}

export function createFlickrClient(opts: FlickrClientOptions) {
  const f = opts.fetchImpl ?? fetch;
  const base = opts.baseUrl.replace(/\/$/, "");

  async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await f(`${base}${path}`, {
      method,
      headers: {
        "x-organization-id": opts.organizationId,
        ...(body !== undefined ? { "content-type": "application/json" } : {}),
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
    const json = (await res.json().catch(() => ({}))) as Record<string, unknown>;
    if (!res.ok) {
      // 412 = Flickr token 未登録 (要 /oauth フロー)、424 = Flickr API 上流エラー
      throw new FlickrApiError(res.status, json as { error?: string; message?: string });
    }
    return json as T;
  }

  return {
    /** OAuth 認可 URL を発行 (request token は flickr_oauth_sessions にも保存される) */
    oauthUrl: () => request<OauthUrlResponse>("GET", "/oauth/url"),
    /** 認可後の verifier を access token に交換して保存 */
    oauthCallback: (body: OauthCallbackRequest) =>
      request<OauthCallbackResponse>("POST", "/oauth/callback", body),
    /** 未検証 cam_files.flickr_id の検証 + flickr_photo 登録 */
    importPhotos: (body: ImportRequest = {}) =>
      request<ImportResponse>("POST", "/import", body),
  };
}
