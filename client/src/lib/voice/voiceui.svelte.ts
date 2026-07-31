// Open/close flags for the voice device pickers (camera device + the desktop
// screen-share source picker). Shared so the voice control buttons (VoiceBar /
// VoiceStage) can open the picker modals, which live at the app root.
//
// `screenMenu` is the Discord-style popover shown while a share is live — it
// carries the anchor rect of the button that opened it, so the menu can sit
// against whichever control bar was clicked.
export const voiceUI = $state<{
  cameraPicker: boolean;
  screenPicker: boolean;
  screenMenu: { left: number; bottom: number } | null;
}>({
  cameraPicker: false,
  screenPicker: false,
  screenMenu: null,
});

/** Open the share menu anchored above `el` (the clicked control button). */
export function openScreenMenu(el: HTMLElement) {
  const r = el.getBoundingClientRect();
  voiceUI.screenMenu = {
    left: r.left + r.width / 2,
    bottom: window.innerHeight - r.top + 8, // sit above the button
  };
}
