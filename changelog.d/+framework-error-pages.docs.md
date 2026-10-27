The Flask and Django guide no longer says a Flask client sees Muru's error
page for an exception: Flask answers with its own page unless
`PROPAGATE_EXCEPTIONS` is set, and Django unless `DEBUG_PROPAGATE_EXCEPTIONS`
is set, as the guide now shows.
