// Open/close flag for the camera device picker. Shared so the voice control
// buttons (in VoiceBar / VoiceStage) can open the picker modal, which lives at
// the app root. (Screen sharing uses the OS picker, so it needs no flag.)
export const voiceUI = $state<{ cameraPicker: boolean }>({
  cameraPicker: false,
});
