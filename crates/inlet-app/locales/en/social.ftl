# Friends, friend requests, blocks, the DM list's actions and group DMs.

## The DM list

social-friends = Friends
social-personal-notes = Personal notes
social-pin-dm = Pin DM
social-unpin-dm = Unpin DM
social-close-dm = Close DM
social-group-settings = Group settings

## The friends page

social-tab-online = Online
social-tab-all = All
social-tab-pending = Pending
social-tab-blocked = Blocked
social-add-friend = Add Friend
social-search = Search
social-online-count = Online — { $count }
social-all-count = All friends — { $count }
social-pending-count = Pending — { $count }
social-blocked-count = Blocked — { $count }
social-empty-online = No friends are online right now.
social-empty-all = No friends yet. Add one with their username.
social-empty-pending = There are no pending friend requests.
social-empty-blocked = You have not blocked anyone.
social-incoming-request = Incoming friend request
social-outgoing-request = Outgoing friend request
social-blocked = Blocked
social-message = Message
social-accept-request = Accept Friend Request
social-cancel-request = Cancel Friend Request
social-unblock = Unblock

## Adding a friend

social-add-friend-body = You can add friends with their username.
social-add-friend-placeholder = Username, or username#0000
social-add-friend-send = Send Friend Request
social-add-friend-sent = Your friend request to { $name } was sent.
social-add-friend-invalid = Enter a username of up to 32 characters, with an optional #tag of 1-4 digits.
social-sending = Sending…

## Confirmations

social-remove-friend = Remove Friend
social-remove-friend-title = Remove { $name }?
social-remove-friend-body = Are you sure you want to remove { $name } from your friends?
social-block = Block
social-block-title = Block { $name }?
social-block-body = { $name } will be removed from your friends, and their messages will be hidden.
social-leave-group = Leave Group
social-leave-group-title = Leave { $name }?
social-leave-group-body = You will not be able to rejoin { $name } unless you are added again.

## Group DMs

social-new-dm-title = Select friends
social-new-dm-body = You can add { $count } more { $count ->
    [one] friend.
   *[other] friends.
}
social-new-dm-open = Open DM
social-new-group-create = Create Group DM
social-group-dm-search = Type the username of a friend
social-group-no-friends = No friends to add.
social-group-add-title = Add friends to { $name }
social-group-add-body = Pick a friend to add to the group.
social-group-add = Add Friends
social-group-name = Group name
social-group-name-placeholder = Group name
social-group-icon-prompt = Choose a group icon
social-group-icon-change = Change icon
social-group-icon-remove = Remove icon
social-group-members = Members — { $count }
social-group-owner = Owner
social-save = Save
social-close = Close
social-group-dm-captcha = This instance wants a CAPTCHA that Inlet could not solve. Create the group in your browser, or try again later.
social-group-dm-rate-limited = You have created or changed too many groups for now. Try again in { $wait }.
social-group-dm-limit = Too many group changes for now. Try again in { $wait }.
social-group-dm-not-addable = These people cannot be added: { $names }.
social-group-dm-failed = The group could not be created.
social-icon-too-large = The picture is larger than 10 MB.
social-icon-not-an-image = That file is not a PNG, JPEG, GIF or WebP picture.

## Messages from blocked people

social-blocked-messages = { $count } blocked { $count ->
    [one] message
   *[other] messages
}
social-blocked-messages-hide = { $count } blocked { $count ->
    [one] message
   *[other] messages
} — hide

## Failures and waits

social-busy = That is already on its way.
social-action-failed = That did not work. Try again.
social-too-fast = Slow down a little, then try again.
social-not-connected = Not connected.
social-wait-seconds = { $count } { $count ->
    [one] second
   *[other] seconds
}
social-wait-minutes = { $count } { $count ->
    [one] minute
   *[other] minutes
}
social-wait-hours = { $count } { $count ->
    [one] hour
   *[other] hours
}
