# Desktop notifications and their sound (backend/notifications/alerts.rs,
# ui/notifications/alerts.rs, ui/settings/tabs/notifications.rs).

alerts-title-guild = { $author } (#{ $channel }, { $guild })
alerts-title-group = { $author } ({ $group })
alerts-title-account = { $title } · { $account }
alerts-group-dm = Group
alerts-hidden-message = New message
alerts-hidden-mention = Mentioned you
alerts-attachment = Sent an attachment
alerts-attachments = Sent { $count } attachments
alerts-embed = Sent an embed
alerts-spoiler = (spoiler)
alerts-unknown-user = unknown user
alerts-unknown-role = unknown role
alerts-unknown-channel = unknown channel
alerts-test-title = Inlet
alerts-test-body = Notifications are working.

settings-notifications-title = Notifications
settings-notifications-desktop = DESKTOP NOTIFICATIONS
settings-notifications-enabled = Show desktop notifications
settings-notifications-preview = Show the message in notifications
settings-notifications-preview-note = When this is off, a notification only says who wrote and where.
settings-notifications-flash = Flash the taskbar button while Inlet is in the background
settings-notifications-sounds = SOUNDS
settings-notifications-sound = Play a sound for messages that notify
settings-notifications-rules = WHICH MESSAGES NOTIFY
settings-notifications-rules-note = Choose all messages, only @mentions or nothing per server and channel with the bell in the channel header. Muted servers and channels never notify. In a server with more than 250 members, all messages notify only while that server is open; otherwise only @mentions reach Inlet.
settings-notifications-dnd-note = Do Not Disturb silences notifications and sounds for that account.
settings-notifications-every-account = Messages of every signed-in account notify, and a click opens that account and channel.
settings-notifications-test = Send a test notification
