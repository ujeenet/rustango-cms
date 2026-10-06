/**
 * #656 — a rejected form submission leaves no upload on disk. Files used
 * to be written while the body was parsed, before the honeypot and
 * required-field checks, so every bounced submission orphaned one.
 */
import { expect, test } from "@playwright/test";
import { existsSync, readdirSync } from "node:fs";
import { ids } from "./helpers/fixture";

const UPLOADS = ".state/var/form-uploads";

function stored(name: string): string[] {
  if (!existsSync(UPLOADS)) return [];
  return readdirSync(UPLOADS, { recursive: true }).map(String).filter((f) => f.endsWith(name));
}

test("a honeypot-tripped submission stores none of its files", async ({ request }) => {
  await request.get("/");
  const csrf = (await request.storageState()).cookies.find((c) => c.name === "rustango_csrf")?.value ?? "";
  const name = `orphan-${Date.now()}.txt`;
  const res = await request.post(`/forms/submit/${ids().snippet.form}`, {
    headers: { "X-CSRF-Token": csrf },
    multipart: {
      _csrf: csrf,
      _hp: "i am a bot",
      email: "bot@example.test",
      attachment: { name, mimeType: "text/plain", buffer: Buffer.from("spam") },
    },
    maxRedirects: 0,
  });
  expect(res.status(), await res.text()).toBeLessThan(400);
  expect(stored(name), "nothing reached disk").toEqual([]);
});

// #666 — the submit route carries its own body limit, so a host that lifts
// axum's default for its own uploads doesn't lift it here too.
test("a submission over the route's limit is refused", async ({ request }) => {
  await request.get("/");
  const csrf = (await request.storageState()).cookies.find((c) => c.name === "rustango_csrf")?.value ?? "";
  const res = await request.post(`/forms/submit/${ids().snippet.form}`, {
    headers: { "X-CSRF-Token": csrf },
    multipart: {
      _csrf: csrf,
      email: "big@example.test",
      attachment: { name: "big.bin", mimeType: "application/octet-stream", buffer: Buffer.alloc(11 * 1024 * 1024) },
    },
    maxRedirects: 0,
  });
  expect(res.status()).toBe(413);
});
