# fake-oauth for the e2e Docker profile (backend/docker-compose.e2e.yml).
FROM golang:1.21-alpine AS build
WORKDIR /src
COPY go.mod ./
COPY *.go ./
RUN CGO_ENABLED=0 go build -trimpath -o /fake-oauth .

FROM alpine:3.20
# The system roots are copied into bundle.pem next to the test CA.
RUN apk add --no-cache ca-certificates
COPY --from=build /fake-oauth /fake-oauth
EXPOSE 8080 443
ENTRYPOINT ["/fake-oauth", "-http", ":8080", "-https", ":443", "-out", "/trust"]
