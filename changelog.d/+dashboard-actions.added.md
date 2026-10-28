Added the dashboard actions beside maintenance mode: a session that may
act can promote a ready release, roll an application back, drain a
release, pin a worker group's count or unpin it, and restart a worker,
each a form on the page or a POST to `/actions/NAME` for a script, and
each recorded as a `dashboard.action` event.
