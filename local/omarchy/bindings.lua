-- Dictation is non-modal: workspace navigation and application keys stay live.
-- Plain Escape belongs to apps. Ctrl+Esc twice cancels, with corral recovery.
o.bind("CTRL + ESCAPE", "Cancel dictation (press twice)", "voxtype record cancel --double-tap", { submap_universal = true })
hl.unbind("SUPER + CTRL + X")
o.bind("SUPER + CTRL + X", "Toggle dictation", "voxtype record toggle", { submap_universal = true })
hl.unbind("F9")
o.bind("F9", "Start dictation (push-to-talk)", "voxtype record start", { submap_universal = true })
o.bind("F9", "Stop dictation (push-to-talk)", "voxtype record stop", { release = true, submap_universal = true })
