docker run --rm \
	-p 4317:4317 \
	-p 4318:4318 \
	-p 9464:9464 \
	-v "$(pwd)/otel.local.yaml":/etc/otelcol/config.yaml:ro \
	otel/opentelemetry-collector-contrib:latest \
	--config /etc/otelcol/config.yaml
