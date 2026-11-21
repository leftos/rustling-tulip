# Phone text controls

Design for RT-2036: the phone's Chat composer edits like an iOS text field. Owner ask: "In the mobile app, make it so the chat mode input box supports all the usual ios controls, select, paste, etc". Everything below beyond that ask is an orchestrator decision, open to the owner.

## Where the controls live

GPUI's iOS layer already bridges `UITextInput` (marked text, selection, caret rects, the tokenizer), so typing, autocorrect, predictive text, dictation's text and the space-bar caret drag reach `TextInput` through `EntityInputHandler`. What it lacks is every touch editing control: its input view is a hidden 1x1 view and the text is painted by GPUI, so UIKit draws no handles, menu or magnifier. The controls are therefore painted by the app in `TextInput`, as the phone's read-only chat selection already paints its handles and Copy button. A native edit menu and magnifier through the GPUI fork is a later item.

## Behaviour (touch editing on, which only the phone's composer turns on)

- **Double tap** on a word selects the word.
- **Long press** on text selects the word under it and opens the edit bar; on empty space or an empty field it places the caret there and opens the edit bar.
- **Handles**: a selection draws a handle at each end, as the chat selection's handles look; dragging one extends or shrinks the selection, and the field scrolls when a handle reaches its top or bottom edge.
- **Caret drag**: a drag that starts on the caret moves it, following the finger.
- **Tap on the caret** of the focused field opens the edit bar; any other tap closes it and places the caret as today.
- **Edit bar**: a small bar drawn just above the selection (below it when there is no room above), with Cut and Copy while text is selected, Paste always, and Select All while the field holds text that is not all selected. Each closes the bar after acting; typing, a scroll or a tap elsewhere closes it too. Paste never reads the clipboard to decide whether to show itself, so iOS shows its paste prompt only when Paste is pressed.
- **Keyboard**: the composer turns on autocorrect, predictive text and sentence capitalisation (`TextInputConfiguration`).
- Multi-line paste keeps its line breaks (already normalised by `paste_clipboard`).

Desktop text boxes are unchanged: touch editing is off there.

## Steps

- [ ] RT-2036: touch editing in `TextInput`, the edit bar, the composer's keyboard configuration, specs and a shot
