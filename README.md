# Chilsonite

## Overview

Chilsonite is an open‑source rotating proxy system, similar to commercial services like VIP72. It allows you to route your traffic through various proxy nodes (Agents) distributed across different locations, providing anonymity and IP rotation capabilities.

> **Note:** This software was developed for research purposes and may not be suitable for production environments.

## What Is a Rotating Proxy?

A rotating proxy is a service that routes your internet traffic through different IP addresses, which can help with:

- Web scraping without IP blocks
- Testing geo‑restricted content
- Enhancing privacy

Instead of using a single proxy server, the system “rotates” your connection through different proxy nodes (Agents), either randomly or based on specified criteria such as geographic location.

## How to Run

### CServer Setup

The **CServer** (central server) component depends on PostgreSQL for user management and token storage. Currently, it’s not optimized for production deployment.

1. **Configure** `chilsonite.toml`:

   ```toml
   websocket_port = 3005
   socks5_port = 1080
   bind_address = "0.0.0.0"
   connect_timeout_seconds = 30
   ```

- `websocket_port`: Port for communication with Agents.
- `socks5_port`: Port for SOCKS5 proxy connections.
- `bind_address`: Address to bind the server to (default is all interfaces).
- `connect_timeout_seconds`: Timeout for connecting to Agents.

2. **Configure** a `.env` file:

   ```env
   DATABASE_URL=postgres://username:password@localhost/chilsonite
   JWT_SECRET=your_jwt_secret_here
   ```

3. **Run the CServer**:

   ```bash
   ./cserver
   ```

> Future development plans include Docker Compose support to simplify deployment.

### Agent Setup

The **Agent** component runs on proxy nodes and connects to the central server:

- **Run with default local CServer**:

  ```bash
  ./agent
  ```

- **Or specify a custom CServer URL**:

  ```bash
  ./agent ws://your-cserver-address:3005
  ```

Both components can be downloaded from the project’s [Releases](./releases) page.

## How to Use

### User Registration (curl)

```bash
curl -X POST http://localhost:8080/api/register \
  -H "Content-Type: application/json" \
  -d '{"username":"your_username","password":"your_password"}'
```

### User Login (curl)

```bash
curl -X POST http://localhost:8080/api/login \
  -H "Content-Type: application/json" \
  -d '{"username":"your_username","password":"your_password"}' \
  -c cookies.txt
```

### Token Generation (curl)

Tokens are required for proxy authentication and consume points from your account.

```bash
curl -X POST http://localhost:8080/api/token \
  -H "Content-Type: application/json" \
  -b cookies.txt
```

The response will contain your proxy token and its expiration timestamp:

```json
{
  "token": "token_string_here",
  "expires_at": 1234567890
}
```

> If you’re not comfortable with curl commands, it’s recommended to use the Chilsonite Dashboard interface for easier management.

## Proxy Usage

### Agent ID‑Specific Proxy

- **SOCKS5 Proxy:** `localhost:1080`
- **Username:** `agent_xxxxxxxxxxxxx`
- **Password:** `your_token`

```bash
curl -x socks5h://agent_xxxxxxxxxxxxx:your_token@localhost:1080 ifconfig.co/json
```

### Country Code‑Specific Proxy

To connect through an Agent in a specific country, use a country‑code pattern as the username:

- **SOCKS5 Proxy:** `localhost:1080`
- **Username:** `country_JP`
- **Password:** `your_token`

```bash
curl -x socks5h://country_JP:your_token@localhost:1080 ifconfig.co/json
```

You can also specify multiple country codes: `country_JPUS` will select an Agent from either Japan or the United States.

```bash
curl -x socks5h://country_JPUS:your_token@localhost:1080 ifconfig.co/json
```

## Internal Mechanism

### CServer and Agent Communication

Communication between the CServer and Agents is established using the WebSocket protocol. This allows persistent, bidirectional connections for efficient proxy request handling.

### API System

The system provides RESTful API endpoints for user registration, authentication, token generation, and Agent management. These endpoints enable programmatic interaction with the proxy service.

### SOCKS5 System

The SOCKS5 proxy implementation handles client connection requests, authenticates them using tokens, and routes traffic through the appropriate agent based on selection criteria.
