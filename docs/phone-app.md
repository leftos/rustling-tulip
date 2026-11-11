# The phone app

The phone app is the iPhone web app that shows your PC's sessions: Home with its Needs you tray, a session's chat and terminal, and push notifications when a session wants you. It is a web page the PC serves over your tailnet, which you add to the iPhone's Home Screen; nothing comes from the App Store.

## What you need

- An iPhone on iOS 16.4 or later, since Home Screen web apps get push from 16.4.
- Tailscale on the iPhone, signed in as the same Tailscale user as the PC. The phone has to be on the tailnet to open the app and to talk to the PC; a push reaches it without the tailnet.
- HTTPS certificates turned on for the tailnet in the Tailscale admin console (DNS page).
- Remote access turned on on the PC.

## Turn on Remote access on the PC

Open Settings → Remote access on the PC and turn on the switch `Allow remote access over Tailscale`. When the line reads `Serving on your tailnet.`, the section shows the address the phone opens: `Address: https://<host>:8787`, where `<host>` is the PC's MagicDNS name. The port is 8787 unless you changed it in the `Port` field.

If the line says something else, such as `Tailscale is not running. Start it and remote access connects on its own.`, fix that first; [remote-hand-checklist.md](remote-hand-checklist.md) section 1 lists each message.

## Install from Safari

1. On the iPhone, with Tailscale on, open the address in Safari.
2. Tap Share, then Add to Home Screen.
3. Open the app from its new icon on the Home Screen.

Safari and the Home Screen app keep separate storage, so pair from the Home Screen app, never from Safari. Pairing in Safari leaves the Home Screen app unpaired.

## Pair

The first time the Home Screen app opens, it shows the pairing screen, `Pair this phone`, with the line `Ask your computer to let this phone in.` and the button `Ask to pair`. Either of these finishes it.

**Approve on the PC.** Tap `Ask to pair`. The screen says `Approve this phone on your computer, then scan the code it shows.` On the PC, a Windows toast `A device asks for remote access` appears, and Settings → Remote access shows the phone under Waiting devices. Press `Approve…` on the toast, then `Approve` in the dialog. The PC shows a QR code and a six-digit code. The phone says `Scan the code your computer shows, or type it.`: point the phone's camera at the QR code, or type the six digits into the `Six-digit code` field and tap `Pair`. If the camera is off for the app the screen says `The camera is off for this app. Type the code instead.` and if the phone has none, `No camera found. Type the code instead.`

**Mint a code on the PC.** On the PC, run `rustling-tulipd pair` (the daemon has to be running). It prints `Pairing code: <six digits>`. Tap `Ask to pair` on the phone, type those six digits into the `Six-digit code` field and tap `Pair`. No approval on the PC is needed, and the code works once, for one device within the minutes it names.

When the phone is paired the screen says `Paired. Connecting…` and then Home opens. If the PC turns the request down, the phone shows `Ask your computer to let this phone in.` again with `Ask to pair`.

## Notifications

On Home, tap the header's `Notifications` button. The panel opens over Home, headed `Notifications`, and `Done` closes it.

- `Allow notifications` is the main switch. Turning it on asks iOS for permission the first time, subscribes the phone and registers it with the PC; turning it off makes the PC forget the phone. Until the panel has heard from the PC the switch is disabled.
- `Needs you` sends a push when a session starts waiting on you.
- `Finished` sends a push when a session's turn ends.
- `Hold while I'm at my computer` holds the pushes while you are at the PC. You are at the desk when a desktop window of the app is focused and the PC has had keyboard or mouse input in the last 3 minutes. A held `Needs you` push arrives when you walk away, if the session still needs you; a held `Finished` push is dropped. Turn the switch off and every push arrives at once.

The three switches appear once notifications are on, and all start on. The panel shows `Add Rustling Tulip to your Home Screen to get notifications: tap Share, then Add to Home Screen.` when it runs in a Safari tab, and `Notifications are blocked for this app. Turn them on in Settings, then come back.` when iOS permission is denied.

The Home Screen icon carries a badge: the number of sessions that need you. It clears when none does.

## Launch last

A repo or workspace whose last launch on the PC was not headless shows a `Launch last` button at the right of its group header on Home, when the PC's daemon is new enough to offer it. Tap it to start that launch again; the button reads `Launching…` and is disabled until the PC answers, and while the bar says `Reconnecting…`.

A refusal shows the PC's own text in red under the group header. A repo or workspace with a stored launch and no sessions is listed in a `Launch again` group at the end, each row with the same button. The phone stays on Home after a launch.

A launch tapped while the phone is offline is kept and sent again each time the connection comes back, with the same request, so a flaky link starts one session.

A launch that waits a minute with no answer, or comes back after nine minutes, is dropped with `This launch may have started a session. Check the list before launching again.`, since the PC no longer remembers its request by then. A PC too old to launch from the phone drops it with `This host can't launch from the phone.` Closing or reloading the app forgets a launch that was still waiting.

## New session

Home shows a `New session` button below the bar and the Needs you cards and above the groups, once the session list has loaded and when the PC's daemon is new enough to offer it. It is disabled while the bar says `Reconnecting…`. Tap it to open or close a list with one row per workspace and per repo that is not in a workspace, in Home's order, whether or not it was launched before.

A row's `Start` begins a session there with `/nextup` as its first prompt. The row reads `Starting…` and is disabled until the PC answers, and that repo or workspace's `Launch last` is disabled meanwhile (it keeps its label); while a `Launch last` waits, the row's `Start` is disabled, so a repo or workspace has one launch waiting at a time. The list stays open after a start and across reconnects.

A refusal shows the PC's own text in red under its row, and so do the two dropped-start texts below; none shows under a group header.

A start tapped while the phone is offline is kept and sent again each time the connection comes back, with the same request and the same nine-minute and one-minute rules as `Launch last`. A PC too old to start from the phone drops it with `This host can't launch from the phone.`, shown under that repo or workspace's name where the `New session` button would be, with no button. Closing or reloading the app forgets a start that was still waiting.

## Presets

Each repo or workspace group header on Home has a `Presets` button that opens and closes the group's list of presets. The first open asks the PC for them, and an open list is asked again when the connection comes back. A refused list shows the PC's own error inside the open list.

Each row shows the preset's name and a `Launch` button, or a disabled `Needs the desktop` button. A preset can launch from the phone when it needs no input: one of its prompt sources is a folder or inline text with no `{prompt}` in its template, and it has no ask-at-launch, script or unknown variable and no bypass agent option. Any other preset is launched from the desktop.

While a launch runs the row reads `Launching <launched>/<total>` with a `Cancel` button. A failed launch shows its error under the row, and a cancelled one shows `Cancelled`.

A launch the phone sent again after a reconnect keeps its job, so a flaky link starts one batch: an unanswered launch, or a running one under nine minutes old, is resent only to a PC that offers it. An older PC shows `This host can't resend a preset launch; check the desktop.`. A launch that waited too long shows `This launch may have started a session. Check the list before launching again.`, and one the PC no longer knows shows `Lost track of this launch; check the desktop for its sessions.`

## When something is off

**The phone asks to pair again.** iOS can evict the app's storage, and the PC refuses a phone it has blocked or forgotten. Settings → Remote access on the PC lists the phone under `Devices`: when it shows `Blocked`, press `Forget`; when it is not listed, there is nothing to forget. Then tap `Ask to pair` and approve again as above.

**No pushes.**

- Check `Allow notifications` and the `Needs you` or `Finished` switch in the panel, and iOS's own notification setting for the app in Settings.
- Check `Hold while I'm at my computer`: a PC window in use holds the pushes until you walk away.
- After deleting and re-adding the app to the Home Screen, open it once and turn `Allow notifications` on again, since the new install has no subscription yet.
- The PC needs outbound HTTPS to Apple's push service.

**The page cannot load.** Check that Tailscale is on in the iPhone and signed in as the PC's Tailscale user, that Remote access is on and reads `Serving on your tailnet.` on the PC, and that the address uses the port Settings → Remote access shows, which is 8787 unless it was changed. Changing the port or the PC's name makes a new address: add that one to the Home Screen and pair again.

By-hand checks of all of this on a real iPhone are in [remote-hand-checklist.md](remote-hand-checklist.md) section 13. The app's design is in [plans/mobile-app.md](plans/mobile-app.md) and its source layout in [../apps/pwa/README.md](../apps/pwa/README.md).
