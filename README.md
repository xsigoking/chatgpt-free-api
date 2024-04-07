# ChatGPT Free API

Unlimited free GPT-3.5-Turbo API service via reverse login-free ChatGPT Web.

## Features

- **Streaming Response**: The API supports streaming response, so you can get the response as soon as it's available.
- **API Endpoint Compatibility**: Full alignment with official OpenAI API endpoints, ensuring hassle-free integration with existing OpenAI libraries.
- **Complimentary Access**: No charges for API usage, making advanced AI accessible to everyone even without an API key.

## Install

### With docker

```
docker run -p 3040:3040 xsigoking/chatgpt-free-api
```

### Binaries for macOS, Linux, and Windows

Download it from [GitHub Releases](https://github.com/xsigoking/chatgpt-free-api/releases), unzip, and add aichat to your `$PATH`.

## Usage

### Run server

```sh
chatgpt-api-server                                 # Listening on 0.0.0.0:3040, no proxy
PORT=8080 chatgpt-api-server                       # Use $PORT to change the listening port
ALL_PROXY=http://localhost:18080 chatgpt-api-server # Use $ALL_PROXY to set the proxy server
```

### Request Example

```sh
curl http://127.0.0.1:3040/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer any_string_you_like" \
  -d '{
    "model": "gpt-3.5-turbo",
    "messages": [
      {
        "role": "user",
        "content": "Hello!"
      }
    ],
    "stream": true
  }'
```

## License

MIT License