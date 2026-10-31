## Shared elapsed times and inflected display nouns.

common-time-now = just now
common-time-minutes = { $count ->
    [one] { $count } minute ago
   *[other] { $count } minutes ago
    }
common-time-hours = { $count ->
    [one] { $count } hour ago
   *[other] { $count } hours ago
    }
common-time-days = { $count ->
    [one] { $count } day ago
   *[other] { $count } days ago
    }
common-time-months = { $count ->
    [one] { $count } month ago
   *[other] { $count } months ago
    }
common-time-years = { $count ->
    [one] { $count } year ago
   *[other] { $count } years ago
    }
web-count-session = { $count ->
    [one] session
   *[other] sessions
    }
web-count-event = { $count ->
    [one] event
   *[other] events
    }
web-count-observation = { $count ->
    [one] observation
   *[other] observations
    }
web-count-result = { $count ->
    [one] result
   *[other] results
    }
web-count-message = { $count ->
    [one] message
   *[other] messages
    }
web-count-project = { $count ->
    [one] project
   *[other] projects
    }
web-count-page = { $count ->
    [one] page
   *[other] pages
    }
web-count-version = { $count ->
    [one] version
   *[other] versions
    }
web-count-guide = { $count ->
    [one] guide
   *[other] guides
    }
web-count-captured-session = { $count ->
    [one] captured session
   *[other] captured sessions
    }
web-count-sanitized-event = { $count ->
    [one] sanitized event
   *[other] sanitized events
    }

common-status-active = active
common-status-interrupted = interrupted
common-status-completed = completed
common-status-empty = empty
common-status-pending = pending
common-status-claimed = collected
common-status-cancelled = cancelled
common-status-editing = editing
common-status-disabled = disabled
common-status-unknown = unknown
common-status-ok = ok
common-status-error = error
common-status-warn = warn
common-status-open = open
common-status-ended = ended
common-status-revoked = revoked

## Fixed protocol errors. Wire values remain English.
common-error-invalid-credentials = invalid credentials
common-error-auth-unavailable = authentication unavailable
common-error-password-confirmation = password confirmation does not match
common-error-password-reserved = password is reserved
common-error-password-policy = invalid password: does not meet policy
common-error-csrf = csrf required
common-error-password-change = password change required
common-error-auth-required = auth required
common-error-login-rate = too many login attempts
common-error-cross-origin = Cross-origin form request refused
common-error-management-password = Password change required
common-error-admin = Admin access required
common-error-password-length = Use at least 12 characters.
common-error-scope-pair = Both workspace and project are required
common-error-session-required = Session ID required
common-error-files-remain = Database deletion completed, but some wiki files could not be removed. Ask an Admin to check the server logs.
