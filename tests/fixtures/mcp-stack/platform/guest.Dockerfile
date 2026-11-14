# Fixture tail appended to the unchanged deploy/demo/Dockerfile, not a standalone recipe.
# The guest runs as root. This on-disk directory survives snapshot and resume.
RUN install -d -m 0700 /tmp/mcp-stack-fixture
