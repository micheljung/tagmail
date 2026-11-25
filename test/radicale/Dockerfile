FROM python:3.13-alpine
RUN pip install --no-cache-dir radicale==3.9.2 \
 && adduser -D -u 1000 radicale \
 && mkdir -p /data/collections \
 && chown -R radicale:radicale /data
COPY config users /etc/radicale/
USER radicale
EXPOSE 5232
CMD ["python", "-m", "radicale", "--config", "/etc/radicale/config"]
