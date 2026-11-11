# The message composer.

composer-placeholder-empty = Message
composer-placeholder = Message { $channel }
composer-too-long = The message cannot be longer than { $max } characters.

## Autocomplete headings and command descriptions

composer-ac-mentions = Members and roles
composer-ac-channels = Channels
composer-ac-emoji = Emoji
composer-ac-commands = Commands
composer-cmd-shrug = Appends ¯\_(ツ)_/¯ to your message.
composer-cmd-tableflip = Appends (╯°□°)╯︵ ┻━┻ to your message.
composer-cmd-unflip = Appends ┬─┬ ノ( ゜-゜ノ) to your message.
composer-cmd-me = Send an action message (wraps in italics).
composer-cmd-spoiler = Send a spoiler message (wraps in spoiler tags).
composer-cmd-nick = Change your nickname in this community.
composer-cmd-msg = Send a direct message to a user.
composer-cmd-saved = Send a saved media item.

## Commands that call the server

composer-nick-changed = Your nickname in this community is now { $nick }.
composer-nick-reset = Your nickname in this community was removed.
composer-nick-guild-only = Nicknames can only be changed in a community.
composer-msg-sent = Direct message sent.
composer-msg-failed = Could not send the direct message: { $error }
composer-command-failed = The command failed: { $error }
composer-bad-answer = the server gave an answer Inlet could not read
composer-not-connected = not connected to the server
composer-saved-not-yet = Saved media can not be picked here yet.

## Reply bar

composer-replying-to = Replying to
composer-reply-mention-on = @ On
composer-reply-mention-off = @ Off
composer-reply-close = ✕

## Editing a message

composer-edit-title = escape to
composer-edit-enter = • enter to
composer-edit-cancel = cancel
composer-edit-save = save
composer-edit-failed = Could not edit the message: { $error }

## Slowmode

composer-slowmode-on = Slowmode is on
composer-slowmode-wait = Slowmode: { $time }
composer-slowmode-immune = Slowmode does not apply to you

## Attachments

composer-attach-plus = +
composer-attach-prompt = Attach
composer-attach-remove = ✕
composer-attach-spoiler = Spoiler
composer-attach-spoiler-on = SPOILER
composer-attach-alt = ALT
composer-attach-alt-title = Alt text for { $name }
composer-attach-alt-placeholder = Describe the file for people who cannot see it
composer-attach-alt-hint = enter to save • escape to cancel
composer-attach-check = ✓
composer-attach-strip-exif = Remove photo location and camera data before upload
composer-demo-alt = A blue rectangle
composer-pending-files = 📎 { $names }
composer-upload-too-many = A message can carry at most { $max } files.
composer-upload-too-large = { $name } is larger than { $max }, the most this account can upload.
composer-upload-too-much = These files are larger than { $max } together, the most one message can carry.
composer-upload-not-a-file = { $name } is not a file and was not attached.
composer-upload-unreadable = the file could not be read
composer-upload-file-changed = the file changed or could not be read after it was attached
composer-upload-gone = the files are no longer kept; attach them again
composer-upload-progress = { $sent } of { $total }
composer-upload-names-more = { $first } and { $more } more
composer-upload-cancel = Cancel
