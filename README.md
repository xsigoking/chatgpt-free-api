# ChatGPT Free API

Provides free GPT-3.5-Turbo API service through reverse login-free ChatGPT Web.

## Install

### With docker

```sh
docker run -d --name chatgpt-free-api xsigoking/chatgpt-free-api
```

### Binaries for macOS, Linux, and Windows

Download it from [GitHub Releases](https://github.com/xsigoking/chatgpt-free-api/releases), unzip, and add aichat to your `$PATH`.

## Usage

### Run server

```sh
chatgpt-api-server                                  # Listening on 0.0.0.0:3040, no proxy
PORT=8080 chatgpt-api-server                        # Use $PORT to change the listening port
ALL_PROXY=http://localhost:18080 chatgpt-api-server # Use $ALL_PROXY to set the proxy server
```

### Request Example

```sh
curl http://127.0.0.1:3040/v1/chat/completions \
  -i -X POST \
  -H "Content-Type: application/json" \
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

The project is under the MIT License, Refer to the [LICENSE](https://github.com/xsigoking/chatgpt-free-api/blob/main/LICENSE) file for detailed information.