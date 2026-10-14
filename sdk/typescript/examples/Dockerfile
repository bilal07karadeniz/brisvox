# The TypeScript restaurant example as a container: the SDK agent of "Service 2" in
# docs/DEPLOY.md.
#
# The SDK is not on npm, so the image builds it: the build context is the REPOSITORY ROOT (not
# this directory), `npm ci && npm run build` runs on the copied `sdk/typescript`, and the
# compiled `dist` plus the production `node_modules` end up in the final image. Build and run:
#
#   docker build -f sdk/typescript/examples/Dockerfile -t restaurant-agent .
#   docker run --rm -p 8765:8765 -e BRISVOX_AGENT_TOKEN=change-me restaurant-agent
#
# For your own agent, vendor the SDK the same way: build `sdk/typescript` (a clone or a git
# submodule of this repository in your build context), then in your own package
# `npm install ./vendor/brisvox-agent` after copying that package.json, `dist/` and
# `node_modules/` (production only) into `vendor/brisvox-agent`, and import from "@brisvox/agent".
# Replace the example's `dist/examples/restaurant.js` in CMD with your entry point.
#
# The agent listens on all interfaces (the SDK default, 127.0.0.1, is unreachable from another
# container). On Railway with an environment older than 2025-10-16 whose private network is IPv6
# only, override the command with `--host ::` (docs/DEPLOY.md, "Service 2"). The server reaches
# this container with BRISVOX_AGENT_URL=ws://<this container's name>:8765 and the same
# BRISVOX_AGENT_TOKEN. The SDK agent answers a plain HTTP request with `426`, so give the service
# no HTTP health check.

ARG NODE_VERSION=22

FROM node:${NODE_VERSION}-slim AS build
WORKDIR /sdk
COPY sdk/typescript/package.json sdk/typescript/package-lock.json ./
RUN npm ci
COPY sdk/typescript/tsconfig.json ./
COPY sdk/typescript/src ./src
COPY sdk/typescript/tests ./tests
COPY sdk/typescript/examples ./examples
RUN npm run build && npm prune --omit=dev

FROM node:${NODE_VERSION}-slim
ENV NODE_ENV=production
WORKDIR /app
COPY --from=build --chown=node:node /sdk/package.json ./
COPY --from=build --chown=node:node /sdk/node_modules ./node_modules
COPY --from=build --chown=node:node /sdk/dist ./dist
USER node
EXPOSE 8765
CMD ["node", "dist/examples/restaurant.js", "--host", "0.0.0.0", "--port", "8765"]
