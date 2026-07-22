// Open/close flags for the voice device pickers (camera device + the desktop
// screen-share source picker). Shared so the voice control buttons (VoiceBar /
// VoiceStage) can open the picker modals, which live at the app root.
export const voiceUI = $state<{ cameraPicker: boolean; screenPicker: boolean }>({
  cameraPicker: false,
  screenPicker: false,
});
