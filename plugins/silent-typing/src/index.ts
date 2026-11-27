// @description Prevents Discord from sending or showing typing indicators
// @version 2.0.0
// @author mantikafasi

const KEY = "enabled";
const ACTIVE_TINT = 0xff5865f2;
const ICON = "res://ic_mana_chat_eye";

let enabled = phoenix.storage.get<boolean>(KEY);
if (typeof enabled !== "boolean") {
  enabled = true;
}

let stopTyping: Phoenix.Remover | null = null;
let stopRequest: Phoenix.Remover | null = null;
let button: Phoenix.ButtonRemover | null = null;

function buttonLabel(on: boolean): string {
  return on ? "Silent Typing: On" : "Silent Typing: Off";
}

function syncHooks(on: boolean) {
  if (on) {
    if (!stopTyping) {
      stopTyping = phoenix.messages.suppressTyping();
    }
    if (!stopRequest) {
      stopRequest = phoenix.discord.onRequest(
        { method: "POST", pathPrefix: "/channels/", pathSuffix: "/typing" },
        () => false,
      );
    }
  } else {
    stopTyping?.();
    stopTyping = null;
    stopRequest?.();
    stopRequest = null;
  }
}

function apply(next: boolean, syncSetting: boolean) {
  enabled = next;
  phoenix.storage.set(KEY, enabled);
  if (syncSetting) {
    settings.enabled = enabled;
  }
  syncHooks(enabled);
  button?.update({
    icon: ICON,
    label: buttonLabel(enabled),
    tint: enabled ? ACTIVE_TINT : null,
  });
}

const settings = phoenix.settings.define({
  enabled: {
    type: "switch",
    title: "Silent typing",
    description: "Hide your typing indicator locally and block outgoing typing requests",
    default: enabled,
    onChange(value) {
      apply(value, false);
    },
  },
});

enabled = settings.enabled;
phoenix.storage.set(KEY, enabled);
syncHooks(enabled);

button = phoenix.ui.addComposerButton({
  icon: ICON,
  label: buttonLabel(enabled),
  tint: enabled ? ACTIVE_TINT : undefined,
  onPress() {
    apply(!enabled, true);
  },
});

phoenix.onUnload(() => {
  button?.();
  button = null;
  syncHooks(false);
});
