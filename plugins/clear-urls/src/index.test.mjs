import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import test from "node:test";
import { runInNewContext } from "node:vm";

const gateway = { messageCreate: {}, messageUpdate: {} };
const hooks = new Map();
let send, request, response;
const phoenix = {
  gateway,
  hook(event, { before }) { hooks.set(event, before); },
  messages: { onSend(handler) { send = handler; } },
  discord: {
    onRequest(filter, handler) { request = handler; },
    onResponse(filter, handler) { response = handler; },
  },
};
const source = readFileSync(new URL("./index.ts", import.meta.url), "utf8");
runInNewContext(stripTypeScriptTypes(source), { phoenix });

const clean = "https://x.com/example/status/123";
const tracked = clean + "?S=20&%54=abc&Mx=2&CT=123&cn=foo&REF_SRC=ref&ref_url=src";

test("removes X tracking case-insensitively, including percent-encoded keys", () => {
  assert.equal(send(tracked), clean);
  assert.equal(send(tracked + "&lang=ja&lang=en#thread"), clean + "?lang=ja&lang=en#thread");
  assert.equal(send(clean + "?%ZZ=keep&t=drop"), clean + "?%ZZ=keep");
});

test("handles X mirror hosts without stripping real parameters on other hosts", () => {
  for (const host of ["twitter.com", "fxtwitter.com", "vxtwitter.com", "fixupx.com", "fixvx.com", "xcancel.com"]) {
    assert.equal(send(`https://WWW.${host}.:443/p?s=1&t=2&mx=3&ct=4&cn=5`), `https://WWW.${host}.:443/p`);
  }
  assert.equal(send("https://x.com/search?q=cats&src=typed_query&f=live"), undefined);
  assert.equal(send("https://x.com.example.net/p?s=1&t=2&mx=3"), undefined);
  assert.equal(send("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=43s&si=drop&feature=share"), "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=43s");
  assert.equal(send("https://youtu.be/video?t=43&si=drop&UTM_x=drop"), "https://youtu.be/video?t=43");
  assert.equal(send("https://tiktok.com/post?t=1&mx=keep"), "https://tiktok.com/post?mx=keep");
  assert.equal(send("https://example.com/page?"), undefined);
  assert.equal(send("https://example.com/page?a=1&"), undefined);
});

test("preserves Discord markup, punctuation, quotes, and URL fragments", () => {
  assert.equal(send(`||<${tracked}>|| and (${tracked}).`), `||<${clean}>|| and (${clean}).`);
  assert.equal(send(`"${tracked}"`), `"${clean}"`);
  assert.equal(send(`[post](${tracked}#thread)`), `[post](${clean}#thread)`);
  assert.equal(send(clean + "#thread?t=keep"), undefined);
  assert.equal(send("plain text"), undefined);
});

test("cleans received messages, embed-only updates, replies, and forwarded messages", () => {
  const message = {
    content: tracked,
    embeds: [{ url: tracked, title: "Post" }, null],
    referenced_message: { content: tracked },
    message_snapshots: [null, { message: { embeds: [{ url: tracked }] } }],
  };
  assert.equal(hooks.get(gateway.messageCreate)(message), undefined);
  assert.equal(message.content, clean);
  assert.deepEqual(message.embeds, [{ url: clean, title: "Post" }, null]);
  assert.equal(message.referenced_message.content, clean);
  assert.equal(message.message_snapshots[1].message.embeds[0].url, clean);

  const update = { embeds: [{ url: tracked }] };
  hooks.get(gateway.messageUpdate)(update);
  assert.equal(update.embeds[0].url, clean);
  assert.doesNotThrow(() => hooks.get(gateway.messageUpdate)(null));
});

test("rewrites only changed message PATCH payloads", () => {
  const body = { content: tracked, flags: 4 };
  assert.equal(request({ path: "/channels/42/messages/99?x=1", json: () => body }).body.content, clean);
  assert.equal(body.flags, 4);
  assert.equal(request({ path: "/channels/42/messages/99", json: () => body }), undefined);
  assert.equal(request({ path: "/channels/42/messages/99/reactions", json: () => ({ content: tracked }) }), undefined);
  assert.equal(request({ path: "/channels/42/messages/99", json: () => null }), undefined);
});

test("cleans successful history reads and leaves API errors and unchanged bodies alone", () => {
  const body = [{ content: tracked }, { content: clean }];
  const read = { path: "/channels/42/messages?limit=10", status: 200, json: () => body };
  assert.equal(response({ ...read, status: 404 }), undefined);
  assert.equal(response({ ...read, path: "/channels/no/messages" }), undefined);
  assert.equal(body[0].content, tracked);
  assert.equal(response(read).body, body);
  assert.equal(body[0].content, clean);
  assert.equal(response(read), undefined);
  assert.equal(response({ ...read, json: () => ({ content: tracked }) }), undefined);
  assert.equal(response({ ...read, body: '[{"content":"no query"}]', json() { throw new Error("should skip json()"); } }), undefined);
});
