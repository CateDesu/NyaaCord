// @description Removes tracking parameters from links you send and receive
// @version 0.1.3
// @author amaanq

const { gateway } = phoenix;

const TRACKERS = [
  /^utm_/,
  /^(ref_src|ref_url|fbclid|gclid|dclid|gbraid|wbraid|msclkid|yclid|twclid|ttclid|igshid|igsh|mc_eid|mc_cid)$/,
  /^(_hsenc|_hsmi|mkt_tok|oly_anon_id|oly_enc_id|vero_id|wickedid|__s)$/,
];

// Short parameter names are tracking only on these hosts. Elsewhere they are
// often real parameters, such as a YouTube start time.
const X = /(^|\.)(twitter\.com|x\.com|fxtwitter\.com|vxtwitter\.com|fixupx\.com|fixvx\.com|xcancel\.com)$/;
const TIKTOK = /(^|\.)tiktok\.com$/;
const YOUTUBE = /(^|\.)(youtube\.com|youtu\.be)$/;
const SCOPED = new Map([
  ["feature", YOUTUBE],
  ["pp", YOUTUBE],
  ["si", /(^|\.)(youtube\.com|youtu\.be|spotify\.com|soundcloud\.com)$/],
]);

const isTracker = (key: string, host: string) =>
  (X.test(host) && ["s", "t", "mx", "ct", "cn"].includes(key))
  || (TIKTOK.test(host) && (key === "s" || key === "t"))
  || SCOPED.get(key)?.test(host)
  || TRACKERS.some((tracker) => tracker.test(key));

const clean = (url: string) => {
  const query = url.indexOf("?");
  const hash = url.indexOf("#");
  if (query === -1 || (hash !== -1 && hash < query)) return url;
  const end = hash === -1 ? url.length : hash;
  const authority = url.slice(url.indexOf("//") + 2).split(/[/?#]/)[0];
  const host = authority.slice(authority.lastIndexOf("@") + 1).split(":")[0].toLowerCase().replace(/\.$/, "");
  const pairs = url.slice(query + 1, end).split("&");
  let stripped = false;
  const kept = pairs.filter((pair) => {
    if (!pair) return false;
    let key: string;
    try {
      key = decodeURIComponent(pair.split("=")[0]).toLowerCase();
    } catch {
      return true;
    }
    if (isTracker(key, host)) {
      stripped = true;
      return false;
    }
    return true;
  });
  if (!stripped) return url;
  return url.slice(0, query) + (kept.length ? `?${kept.join("&")}` : "") + url.slice(end);
};

const cleanLinks = (text: string) => text.replace(/https?:\/\/(?:[^\s<>()`|"]|\([^\s<>()`|"]*\))+/gi, (url) => {
  const suffix = url.match(/(?:[.,!?;:\]]|\*\*|~~)+$/)?.[0] ?? "";
  const link = suffix ? url.slice(0, -suffix.length) : url;
  return clean(link) + suffix;
});

type Message = {
  content?: string;
  embeds?: { url?: string }[];
  referenced_message?: Message | null;
  message_snapshots?: { message?: Message }[];
};

const rewrite = (messages: unknown[]) => {
  let changed = false;
  const pending = [...messages];
  while (pending.length) {
    const next = pending.pop();
    if (next === null || typeof next !== "object") continue;
    const message = next as Message;
    if (typeof message.content === "string" && message.content.includes("?")) {
      const content = cleanLinks(message.content);
      changed = content !== message.content || changed;
      message.content = content;
    }
    if (Array.isArray(message.embeds)) {
      for (const embed of message.embeds) {
        if (typeof embed?.url !== "string" || !/^https?:\/\//i.test(embed.url)) continue;
        const url = clean(embed.url);
        changed = url !== embed.url || changed;
        embed.url = url;
      }
    }
    if (message.referenced_message) pending.push(message.referenced_message);
    if (Array.isArray(message.message_snapshots)) {
      for (const snapshot of message.message_snapshots) pending.push(snapshot?.message);
    }
  }
  return changed;
};

const before = (message: Message) => { rewrite([message]); };
phoenix.hook(gateway.messageCreate, { before });
phoenix.hook(gateway.messageUpdate, { before });
phoenix.messages.onSend((text) => {
  if (!text.includes("?")) return;
  const cleaned = cleanLinks(text);
  if (cleaned !== text) return cleaned;
});

phoenix.discord.onRequest({ method: "PATCH", pathPrefix: "/channels/", pathIncludes: "/messages/" }, (req) => {
  if (!/^\/channels\/\d+\/messages\/\d+(?:[?#]|$)/.test(req.path)) return;
  if (typeof req.body === "string" && !req.body.includes("?")) return;
  const body = req.json<Message>();
  if (body && rewrite([body])) return { body };
});

phoenix.discord.onResponse({ method: "GET", pathPrefix: "/channels/", pathSuffix: "/messages" }, (res) => {
  if (res.status < 200 || res.status >= 300 || !/^\/channels\/\d+\/messages(?:[?#]|$)/.test(res.path)) return;
  if (typeof res.body === "string" && !res.body.includes("?")) return;
  const body = res.json();
  if (Array.isArray(body) && rewrite(body)) return { body };
});
