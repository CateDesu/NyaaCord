// @description Strips profile themes, effects, avatar decorations, and name styles
// @version 0.2.0
// @author amaanq

const { gateway } = phoenix;

const settings = phoenix.settings.define({
  keepOwn: {
    type: "switch",
    title: "Keep my own",
    description: "Leave your own profile theming in place",
    default: true,
  },
});

const THEMING = [
  "avatar_decoration_data",
  "collectibles",
  "display_name_styles",
  "theme_colors",
  "profile_effect",
];

let self: string | undefined;

type Node = Record<string, unknown>;

// A profile keeps its `theme_colors` in `user_profile`, beside the `user` that
// owns it, so nodes without an owner take their parent's.
const strip = (value: unknown, parent?: unknown): boolean => {
  if (Array.isArray(value)) {
    let changed = false;
    for (const item of value) {
      if (strip(item, parent)) changed = true;
    }
    return changed;
  }
  if (typeof value !== "object" || value === null) return false;
  const node = value as Node;
  const owner = (node.user as Node | undefined)?.id ?? node.id ?? parent;
  const keep = settings.keepOwn && self !== undefined && owner === self;
  let changed = false;
  for (const key of Object.keys(node)) {
    if (!keep && THEMING.includes(key)) {
      delete node[key];
      changed = true;
    } else if (strip(node[key], owner)) {
      changed = true;
    }
  }
  return changed;
};

const readSelf = () =>
  phoenix.discord
    .me()
    .then((user) => (self = user?.id))
    .catch((err) => phoenix.log.warn(`reading the account: ${err}`));
readSelf();
if (gateway.ready) {
  phoenix.on(gateway.ready, (ready) => {
    if (ready?.user?.id) self = ready.user.id;
  });
}
if (gateway.userUpdate) {
  phoenix.on(gateway.userUpdate, (user) => {
    if (user?.id) self = user.id;
  });
}
if (gateway.readySupplemental) phoenix.on(gateway.readySupplemental, readSelf);

const events = [
  gateway.readySupplemental,
  gateway.initialGuild,
  gateway.guildCreate,
  gateway.messageCreate,
  gateway.messageUpdate,
  gateway.guildMemberAdd,
  gateway.guildMemberUpdate,
  gateway.guildMembersChunk,
  gateway.guildMemberListUpdate,
  gateway.presenceUpdate,
  gateway.presencesReplace,
  gateway.threadMembersUpdate,
  gateway.channelCreate,
  gateway.relationshipAdd,
  gateway.relationshipUpdate,
];
for (const sig of events) {
  if (sig) {
    phoenix.hook(sig, {
      before: (payload) => {
        strip(payload);
      },
    });
  }
}

// Desktop fetches profiles and message history over HTTP, past the gateway.
const fromResponse = (res: Phoenix.InterceptedResponse) => {
  if (res.status < 200 || res.status >= 300 || !THEMING.some((key) => res.body.includes(key))) return;
  const body = res.json();
  if (body === null || !strip(body)) return;
  return { body: JSON.stringify(body) };
};
phoenix.discord.onResponse({ method: "GET", pathPrefix: "/users/", pathSuffix: "/profile" }, fromResponse);
phoenix.discord.onResponse({ method: "GET", pathPrefix: "/channels/", pathSuffix: "/messages" }, fromResponse);
