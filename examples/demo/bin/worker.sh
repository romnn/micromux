#!/bin/sh
# Structured JSON records in the shape of tracing-subscriber's JSON formatter, so the TUI shows
# them with timestamps and leaves out the source location and span fields the config hides.
cat <<'JSON'
{"timestamp":"2026-09-25T12:00:02.104Z","level":"INFO","fields":{"message":"queue consumer started","queue":"jobs","concurrency":4},"target":"worker::consumer","filename":"src/consumer.rs","line_number":41,"span":{"queue":"jobs","name":"consume"},"spans":[{"queue":"jobs","name":"consume"}]}
{"timestamp":"2026-09-25T12:00:02.311Z","level":"DEBUG","fields":{"message":"claimed job","job_id":1042,"kind":"send_email"},"target":"worker::jobs","filename":"src/jobs.rs","line_number":88,"span":{"job_id":1042,"name":"job"},"spans":[{"queue":"jobs","name":"consume"},{"job_id":1042,"name":"job"}]}
{"timestamp":"2026-09-25T12:00:02.349Z","level":"INFO","fields":{"message":"job finished","job_id":1042,"duration_ms":38},"target":"worker::jobs","filename":"src/jobs.rs","line_number":131,"span":{"job_id":1042,"name":"job"},"spans":[{"queue":"jobs","name":"consume"},{"job_id":1042,"name":"job"}]}
{"timestamp":"2026-09-25T12:00:03.020Z","level":"DEBUG","fields":{"message":"claimed job","job_id":1043,"kind":"resize_image"},"target":"worker::jobs","filename":"src/jobs.rs","line_number":88,"span":{"job_id":1043,"name":"job"},"spans":[{"queue":"jobs","name":"consume"},{"job_id":1043,"name":"job"}]}
{"timestamp":"2026-09-25T12:00:03.207Z","level":"WARN","fields":{"message":"upstream timed out, retrying","job_id":1043,"attempt":2},"target":"worker::jobs","filename":"src/jobs.rs","line_number":117,"span":{"job_id":1043,"name":"job"},"spans":[{"queue":"jobs","name":"consume"},{"job_id":1043,"name":"job"}]}
{"timestamp":"2026-09-25T12:00:03.432Z","level":"INFO","fields":{"message":"job finished","job_id":1043,"duration_ms":412},"target":"worker::jobs","filename":"src/jobs.rs","line_number":131,"span":{"job_id":1043,"name":"job"},"spans":[{"queue":"jobs","name":"consume"},{"job_id":1043,"name":"job"}]}
{"timestamp":"2026-09-25T12:00:04.001Z","level":"INFO","fields":{"message":"queue drained","processed":2,"failed":0},"target":"worker::consumer","filename":"src/consumer.rs","line_number":67,"span":{"queue":"jobs","name":"consume"},"spans":[{"queue":"jobs","name":"consume"}]}
JSON
exec sleep 1000000
