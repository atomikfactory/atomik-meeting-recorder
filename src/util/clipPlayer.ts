// One shared audio player for short voice clips (a speaker in a meeting, or a
// saved voice-profile sample). Only one clip plays at a time: starting a new
// one stops the previous, and playing the active clip again stops it.
// Clips arrive as base64 WAV and are played through an `Audio` element with a
// data: URL, so nothing is written to disk.

let audio: HTMLAudioElement | null = null;
let activeKey: string | null = null;
const listeners = new Set<() => void>();

/// Register a callback fired whenever the playing clip changes (start, end,
/// error) so a view can re-render its Play/Stop buttons.
export function onClipPlaybackChange(listener: () => void) {
  listeners.add(listener);
}

function notify() {
  listeners.forEach((listener) => listener());
}

export function playingClipKey(): string | null {
  return activeKey;
}

export function stopClip() {
  if (audio) {
    audio.onended = null;
    audio.onerror = null;
    audio.pause();
    audio = null;
  }
  if (activeKey !== null) {
    activeKey = null;
    notify();
  }
}

/// Plays `base64Wav` under `key`. Rejects if the browser cannot play it.
export async function playClip(key: string, base64Wav: string): Promise<void> {
  stopClip();
  const next = new Audio(`data:audio/wav;base64,${base64Wav}`);
  audio = next;
  activeKey = key;
  const finish = () => {
    if (audio === next) {
      audio = null;
      activeKey = null;
      notify();
    }
  };
  next.onended = finish;
  next.onerror = finish;
  notify();
  try {
    await next.play();
  } catch (error) {
    finish();
    throw error;
  }
}
