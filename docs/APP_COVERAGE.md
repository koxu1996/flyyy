# App coverage

Real-world images run on the spike, one machine each: deploy, wait for `started`, then a protocol probe
through the worker's backhaul. "Supported" means the probe answered: HTTP status < 500, redis-cli PING,
psql/mariadb `select 1`, mongosh ping, MQTT CONNACK, NATS INFO, AMQP connection.start, Kafka ApiVersions.

Run with `nix build --impure -f compat -L` (compat/, local and gitignored; the images are pre-pulled
into the store by compat/fetch.sh, so the test runs sandboxed). This report comes from the run at
commit 18008d6, every machine at 2 vCPUs / 1024 MiB (`flyyy deploy --cpus 2 --memory 1024`).
The digest is the one deployed from the source registry; the test pulls its linux/arm64 image.

**57 of 62 supported**, 1 not supported, 4 skipped.
Env values are test settings (the passwords match the probes'), not recommendations.

| image | category | ref (repo, digest) | vCPUs | MiB | port | command | env | status | notes |
|---|---|---|---|---|---|---|---|---|---|
| nginx | web | `mirror.gcr.io/library/nginx`<br>`sha256:600f7cd45e27aa13e9304c834ab7c1b0ff39df85ee1dcc5087955455d77bc83b` | 2 | 1024 | 80 | – | – | ✅ supported | probe http:/: OK HTTP 200 |
| caddy | web | `mirror.gcr.io/library/caddy`<br>`sha256:5e5286cb37d44086d651a5363f05f43a692ce4d649d88fb1bd844579f45fa9e8` | 2 | 1024 | 80 | – | – | ✅ supported | probe http:/: OK HTTP 200 |
| httpd | web | `mirror.gcr.io/library/httpd`<br>`sha256:d3accb516506c05ae0068bb54ba6798a10b249b64db91268fca692326cb02b4b` | 2 | 1024 | 80 | – | – | ✅ supported | probe http:/: OK HTTP 200 |
| traefik | web | `mirror.gcr.io/library/traefik`<br>`sha256:1336e85aaed293bd63d005086e0dd013b04078b26aaeb1c18b26a2f928af7e65` | 2 | 1024 | 8080 | `--api.insecure=true` | – | ✅ supported | probe http:/api/version: OK HTTP 200 |
| haproxy | web | `mirror.gcr.io/library/haproxy`<br>`sha256:b26dc654dcd188970e4ba3d478fd92b5b00688f85a81f0a50b1adc63bd3e0793` | – | – | 8404 | – | – | ⏭️ skipped | the image ships no haproxy.cfg; it needs a config file, which a deploy can't provide yet |
| envoy | web | `mirror.gcr.io/envoyproxy/envoy`<br>`sha256:a707c3821b4cecb5db43d8e86e983e0f57b81010fefbabc01feeb071fb8cc08e` | 2 | 1024 | 9901 | – | – | ✅ supported | probe http:/ready: OK HTTP 426 |
| sws | web | `mirror.gcr.io/joseluisq/static-web-server`<br>`sha256:94618e3d17802265a21926217e6f9e7832b311b6fbf5933684001739fafed06a` | 2 | 1024 | 80 | – | – | ✅ supported | probe http:/: OK HTTP 200 |
| postgres | db | `mirror.gcr.io/library/postgres`<br>`sha256:31ba9c94a03ada9988f2bb493a22a75eef3a185884533e5cdbcd498ea835b08f` | 2 | 1024 | 5432 | – | `POSTGRES_PASSWORD=compat` | ✅ supported | probe pg: OK 1 |
| mysql | db | `mirror.gcr.io/library/mysql`<br>`sha256:a0225d1ea6631c8bc2385dc4c2faba17bbf1834ecf369e1659a9a382bd8d5dd7` | 2 | 1024 | 3306 | – | `MYSQL_ROOT_PASSWORD=compat` | ✅ supported | probe mysql: OK 1 |
| mariadb | db | `mirror.gcr.io/library/mariadb`<br>`sha256:c4124d1afa875e47f2892edf26b482fa54f796e2f342d3717a04ca091a22a9bd` | 2 | 1024 | 3306 | – | `MARIADB_ROOT_PASSWORD=compat` | ✅ supported | probe mysql: OK 1 |
| mongo | db | `mirror.gcr.io/library/mongo`<br>`sha256:e16e606f792ea89d2b4907b54e5e31eb86a7a23e27231dce3a982cc8f2d73cd2` | 2 | 1024 | 27017 | – | – | ✅ supported | probe mongo: OK 1 |
| redis | db | `mirror.gcr.io/library/redis`<br>`sha256:4764ce37931aac77c0d033680b339b19b4a4a81772204d268947cb851212d0ac` | 2 | 1024 | 6379 | – | – | ✅ supported | probe redis: OK PONG |
| valkey | db | `mirror.gcr.io/valkey/valkey`<br>`sha256:8f836008e0986c36adf50da714c7060cf689dbb335f61fe077939d90fa650809` | 2 | 1024 | 6379 | – | – | ✅ supported | probe redis: OK PONG |
| keydb | db | `mirror.gcr.io/eqalpha/keydb`<br>`sha256:6537505c42355ca1f571276bddf83f5b750f760f07b2a185a676481791e388ac` | 2 | 1024 | 6379 | – | – | ✅ supported | probe redis: OK PONG |
| memcached | db | `mirror.gcr.io/library/memcached`<br>`sha256:9c50e44b81bda45b1214ed4f3e11a4b96721a05728279775c7f76f7b06a804d6` | 2 | 1024 | 11211 | – | – | ✅ supported | probe memcached: OK b'VERSION 1.6.45' |
| cockroach | db | `mirror.gcr.io/cockroachdb/cockroach`<br>`sha256:ffd6f68e85216912d74581188209159a54a91d0e85b7f32ea260930aa1c06b99` | 2 | 1024 | 26257 | `start-single-node --insecure` | – | ✅ supported | probe pgcrdb: OK 1 |
| clickhouse | db | `mirror.gcr.io/clickhouse/clickhouse-server`<br>`sha256:0152dd511befe6a2c2ef53e930726179669b08116da78500b37c51c96ff5ee77` | 2 | 1024 | 8123 | – | – | ✅ supported | probe http:/ping: OK HTTP 200 |
| couchdb | db | `mirror.gcr.io/library/couchdb`<br>`sha256:bed314d10cdf58c0fff63352db4c3f168df58a185a773c0e2295105375c5ef10` | 2 | 1024 | 5984 | – | `COUCHDB_USER=admin`<br>`COUCHDB_PASSWORD=compat` | ✅ supported | probe http:/: OK HTTP 200 |
| cassandra | db | `mirror.gcr.io/library/cassandra`<br>`sha256:ae4cc2e4b62a5b2a2dd1645ba827cb50c589cc7531bcffe8b8b60ac3b1adc79d` | – | – | 9042 | – | – | ⏭️ skipped | a single node takes longer than the agent's fixed 30 s readiness window to open 9042 |
| etcd | db | `quay.io/coreos/etcd`<br>`sha256:a3ccd299377af4b9968d8cbaaf9722fe354297f11908f74a846075e1e5f4ee89` | 2 | 1024 | 2379 | – | – | ✅ supported | probe http:/health: OK HTTP 200 |
| influxdb | db | `mirror.gcr.io/library/influxdb`<br>`sha256:a450f6faf9573e6c2e96a0f575885ae8b7387cc87e4de94599e247ebfa3ac417` | 2 | 1024 | 8086 | – | – | ✅ supported | probe http:/health: OK HTTP 200 |
| rabbitmq | queue | `mirror.gcr.io/library/rabbitmq`<br>`sha256:485319d42170eab53d8a030e1cc49d95e41f5d325479fa504736db8091981eb7` | 2 | 1024 | 5672 | – | – | ✅ supported | probe amqp: OK connection.start b'\x01\x00\x00\x00\x |
| nats | queue | `mirror.gcr.io/library/nats`<br>`sha256:991242c32d2738ffd358fd6614dd96c735017b4b3b34bfdb2c52a072d66db211` | 2 | 1024 | 4222 | – | – | ✅ supported | probe nats: OK b'INFO {"server_id":"NC5AQ3QGJPOF7MUH |
| redpanda | queue | `mirror.gcr.io/redpandadata/redpanda`<br>`sha256:8b1076e0d61bfceb66594dc01536427ce0b529f8ead35fdab9763ab6085891ce` | 2 | 1024 | 9092 | – | – | ✅ supported | probe kafka: OK ApiVersions ok 290B |
| kafka | queue | `mirror.gcr.io/apache/kafka`<br>`sha256:3f7b939115cd4872e9cee9369d80bd69712fde55f9902f46d793f64848dedc75` | 2 | 1024 | 9092 | – | – | ✅ supported | probe kafka: OK ApiVersions ok 380B |
| mosquitto | queue | `mirror.gcr.io/library/eclipse-mosquitto`<br>`sha256:860aeb5d63ce509508f6dcfb2bc8a1c1dd2bc60555b562b45ce90e52d8dabb0f` | 2 | 1024 | 1883 | – | – | ✅ supported | probe mqtt: OK Client null sending CONNECT Client (n |
| minio | object | `mirror.gcr.io/minio/minio:RELEASE.2025-09-07T16-13-09Z` | – | – | – | – | – | ⏭️ skipped | no public linux/arm64 image any more (MANIFEST_UNKNOWN); SeaweedFS stands in for it |
| seaweedfs | object | `mirror.gcr.io/chrislusf/seaweedfs`<br>`sha256:bb05d66d2963b1cc48073190781c3dd29e2c4c88a2ae2986bf58e38c86d89c6e` | 2 | 1024 | 9333 | `server` | – | ✅ supported | probe http:/cluster/status: OK HTTP 200 |
| meili | search | `mirror.gcr.io/getmeili/meilisearch`<br>`sha256:fe500cf9cca05cb9f027981583f28eccf17d35d94499c1f8b7b844e7418152fc` | 2 | 1024 | 7700 | – | – | ✅ supported | probe http:/health: OK HTTP 200 |
| typesense | search | `mirror.gcr.io/typesense/typesense`<br>`sha256:316b7e71c21f7e5e5caa8daa150e1b3f2be8c876081ee1f77bc2d92cd7f137d0` | 2 | 1024 | 8108 | `--data-dir=/tmp --api-key=x` | – | ❌ not supported | segfaults at start (exit 139) with no output; not investigated yet |
| opensearch | search | `mirror.gcr.io/opensearchproject/opensearch`<br>`sha256:3bfa1ec74d56928c2e018033d45c460ce2c0ea65a5cedd577de3102326c31b89` | 2 | 1024 | 9200 | `opensearch -Ediscovery.type=single-node` | `DISABLE_SECURITY_PLUGIN=true`<br>`DISABLE_INSTALL_DEMO_CONFIG=true`<br>`OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m` | ✅ supported | probe http:/: OK HTTP 200 |
| prometheus | obs | `quay.io/prometheus/prometheus`<br>`sha256:fa3302279710be2f21a8301ec17a8b4f2301fd829bd113839b3b6b02b6f191a2` | 2 | 1024 | 9090 | – | – | ✅ supported | probe http:/-/ready: OK HTTP 200 |
| alertmgr | obs | `quay.io/prometheus/alertmanager`<br>`sha256:e9733bafb1bdef9b00e25a21f8f99dc26a22224bf16641ad754d1649f4c3357a` | 2 | 1024 | 9093 | – | – | ✅ supported | probe http:/-/ready: OK HTTP 200 |
| nodeexp | obs | `quay.io/prometheus/node-exporter`<br>`sha256:1b4e4438faca4dd7e001dd445d161a4a2091b0fededa84093b3a8dfeae1f1be0` | 2 | 1024 | 9100 | – | – | ✅ supported | probe http:/metrics: OK HTTP 200 |
| grafana | obs | `mirror.gcr.io/grafana/grafana`<br>`sha256:74144189b38447facf737dfd0f3906e42e0776212bf575dc3334c3609183adf7` | 2 | 1024 | 3000 | – | – | ✅ supported | probe http:/api/health: OK HTTP 200 |
| loki | obs | `mirror.gcr.io/grafana/loki`<br>`sha256:00981fd9455db8589c3aa6d06744af3138c4b2c32fdec62101e92c7a704b2642` | 2 | 1024 | 3100 | – | – | ✅ supported | probe http:/ready: OK HTTP 200 |
| jaeger | obs | `mirror.gcr.io/jaegertracing/jaeger`<br>`sha256:8c593d8048d86334eb52dba26633b6c846b8ecbe5c22905702c932ae97baeb42` | 2 | 1024 | 16686 | – | – | ✅ supported | probe http:/: OK HTTP 200 |
| otelcol | obs | `mirror.gcr.io/otel/opentelemetry-collector`<br>`sha256:6d260efde7406a1f7d731a5d9663cb6ce25fa634f3ef3ef4a69266aaae1680fd` | 2 | 1024 | 4318 | – | – | ✅ supported | probe http:/v1/traces: OK HTTP 405 |
| vmetrics | obs | `mirror.gcr.io/victoriametrics/victoria-metrics`<br>`sha256:f60dc268b54ecf87f6e09d740c2b732838aa9193090cc741417c9f6386a130df` | 2 | 1024 | 8428 | – | – | ✅ supported | probe http:/health: OK HTTP 200 |
| python | runtime | `mirror.gcr.io/library/python`<br>`sha256:2d44738ebf4ce5f63b9d9ea871e9c7cfdc0426f7b0c97cac915fd88ded1d86bb` | 2 | 1024 | 8000 | `python3 -m http.server 8000` | – | ✅ supported | probe http:/: OK HTTP 200 |
| node | runtime | `mirror.gcr.io/library/node`<br>`sha256:4b150b48b4116e2a22f2c39cc27025fbf4f153e2078fd3b14d159fdb5c7279da` | 2 | 1024 | 3000 | `node -e "require('http').createServer((q,s)=>s.end('ok')).listen(3000)"` | – | ✅ supported | probe http:/: OK HTTP 200 |
| ruby | runtime | `mirror.gcr.io/library/ruby`<br>`sha256:3a8122c4267d89a95b1ce0f3029b44e325180314661856300d29d3e8a80ec46f` | 2 | 1024 | 8000 | `ruby -rsocket -e 's=TCPServer.new(8000);loop{c=s.accept;c.gets;c.print("HTTP/1.0 200 OK\r\n\r\nok");c.close}'` | – | ✅ supported | probe http:/: OK HTTP 200 |
| php | runtime | `mirror.gcr.io/library/php`<br>`sha256:9cf851bc2c1b37c3e59dfd677a3c3e56d485af5a47c93f629424bb039eafcb69` | 2 | 1024 | 8000 | `php -S 0.0.0.0:8000 -t /` | – | ✅ supported | probe http:/: OK HTTP 404 |
| deno | runtime | `mirror.gcr.io/denoland/deno`<br>`sha256:3373fa94f2a9295041f35ec617d13ef5a84eec944235729ec10b5c167a891688` | 2 | 1024 | 8000 | `eval "Deno.serve({port:8000},()=>new Response('ok'))"` | – | ✅ supported | probe http:/: OK HTTP 200 |
| bun | runtime | `mirror.gcr.io/oven/bun`<br>`sha256:1d22548f177f4ff38e0c15baa3c2c46c6c2060ba2d1bc573bb4720b407548f1d` | 2 | 1024 | 3000 | `bun -e "Bun.serve({port:3000,fetch:()=>new Response('ok')})"` | – | ✅ supported | probe http:/: OK HTTP 200 |
| java | runtime | `mirror.gcr.io/library/eclipse-temurin`<br>`sha256:5d90b26c0784ccf55af55f97cbcf7f3969c66a06299c93bdc72649219942a48f` | 2 | 1024 | 8000 | `jwebserver -p 8000 -b 0.0.0.0` | – | ✅ supported | probe http:/: OK HTTP 200 |
| dotnet | runtime | `mcr.microsoft.com/dotnet/samples`<br>`sha256:ef878229733b4c99b2e54a58002df96cfe00ae47288a817a8d8867cbf43a96a0` | 2 | 1024 | 8080 | – | – | ✅ supported | probe http:/: OK HTTP 200 |
| busybox | runtime | `mirror.gcr.io/library/busybox`<br>`sha256:fd7dc98638c8e305f4dc34e979f1c0fdfdcaeb0fbf8fcff77ae834b6da3d7e6e` | 2 | 1024 | 8080 | `httpd -f -p 8080 -h /etc` | – | ✅ supported | probe http:/: OK HTTP 404 |
| registry | app | `mirror.gcr.io/library/registry`<br>`sha256:d4032d4cc323c1731c707931ecada07a53076eac38e3b61ae57533f170f2c6c3` | 2 | 1024 | 5000 | – | – | ✅ supported | probe http:/v2/: OK HTTP 200 |
| gitea | app | `mirror.gcr.io/gitea/gitea`<br>`sha256:918955f16b1e91732af6c449bb2db3a34271748dbed1ccfbae48f8a2fb5480b8` | 2 | 1024 | 3000 | – | – | ✅ supported | probe http:/: OK HTTP 200 |
| vaultwarden | app | `mirror.gcr.io/vaultwarden/server`<br>`sha256:efb3cde962015fcc036b2ea625242248611943b27212ebf4392841fb30fad055` | 2 | 1024 | 80 | – | – | ✅ supported | probe http:/alive: OK HTTP 200 |
| uptimekuma | app | `mirror.gcr.io/louislam/uptime-kuma`<br>`sha256:70233f4acb5163fd2a59a49909cf01e44415cecff13dba69e3a86647a2919f83` | 2 | 1024 | 3001 | – | – | ✅ supported | probe http:/: OK HTTP 302 |
| n8n | app | `mirror.gcr.io/n8nio/n8n`<br>`sha256:9f6d0a5987ab5a297c4b0b38fbf1a22285dd1364551478e93f03d506908fa74c` | 2 | 1024 | 5678 | – | – | ✅ supported | probe http:/healthz: OK HTTP 200 |
| ghost | app | `mirror.gcr.io/library/ghost`<br>`sha256:3e62a20e7f172af9c313bcd74c67098359c4fa7f7121a20ebd740c9f6cd0c0b9` | 2 | 1024 | 2368 | – | `NODE_ENV=development` | ✅ supported | probe http:/: OK HTTP 200 |
| wordpress | app | `mirror.gcr.io/library/wordpress`<br>`sha256:5d2c212561c4b5442ebc4d98933a9cbadcf3dee8888ed3fd9ed44667c27cc905` | 2 | 1024 | 80 | – | – | ✅ supported | probe http:/: OK HTTP 302 |
| nextcloud | app | `mirror.gcr.io/library/nextcloud`<br>`sha256:07ec73cc816e58d6f45a162cd53ef886462c29271a23fc68d0124cec276e3767` | 2 | 1024 | 80 | – | – | ✅ supported | probe http:/status.php: OK HTTP 200 |
| keycloak | app | `quay.io/keycloak/keycloak`<br>`sha256:9409c59bdfb65dbffa20b11e6f18b8abb9281d480c7ca402f51ed3d5977e6007` | 2 | 1024 | 8080 | `start-dev` | – | ✅ supported | probe http:/: OK HTTP 200 |
| adminer | app | `mirror.gcr.io/library/adminer`<br>`sha256:fb9882a47a5c4250e74d18f3e16ca4e09265ff1803d06eb6dde97e3491de7a9b` | 2 | 1024 | 8080 | – | – | ✅ supported | probe http:/: OK HTTP 200 |
| pocketbase | app | `ghcr.io/muchobien/pocketbase`<br>`sha256:9a0be68a80ca1348ba8d15f217b3f983be4c094b3c571a09b3ebdeb357259338` | 2 | 1024 | 8090 | – | – | ✅ supported | probe http:/api/health: OK HTTP 200 |
| umami | app | `ghcr.io/umami-software/umami`<br>`sha256:e69d1a7932e9dab0fc523edc7be1f3a53f3c261f57856fc2167ae74a8fc022e8` | – | – | 3000 | – | – | ⏭️ skipped | needs an external database (DATABASE_URL); apps can't reach each other yet |
| vault | app | `mirror.gcr.io/hashicorp/vault`<br>`sha256:268bb80aa9c6d13d65fcfa05c0c268caca068952240a8087291a6ce0b66e3a10` | 2 | 1024 | 8200 | – | – | ✅ supported | probe http:/v1/sys/health: OK HTTP 200 |
| consul | app | `mirror.gcr.io/hashicorp/consul`<br>`sha256:6126c30072690cb3173a450ff5bde120c3a01e11c9dccab83b1b2314d4c828bd` | 2 | 1024 | 8500 | – | – | ✅ supported | probe http:/v1/status/leader: OK HTTP 200 |

## Demo templates

The 8 templates in the Demos group weren't in the compat run above. Each was deployed from its template
on the live cluster, at the template's defaults (1 vCPU · 128 MiB; thelounge 256 MiB), with 2 machines:
one on worker1 (x86_64) and one on worker2 (aarch64). All started, and a request to its public URL
(`https://<app>.flyyy.devpeek.io`) returned 200 from both machines. For the single-page apps
(excalidraw, it-tools, thelounge), the page's JS bundles loaded too.

**8 of 8 supported.** The digest is the one in templates.json, deployed on both arches.

| image | ref (repo, digest) | vCPUs | MiB | port | command | status | notes |
|---|---|---|---|---|---|---|---|
| whoami | `traefik/whoami`<br>`sha256:c4717a8d1f0134a7444e24f881160e033991f23027c6c5a9a3f8fd22e70d1d44` | 1 | 128 | 80 | – | ✅ supported | HTTP 200 through the public URL, both machines answering |
| echo | `hashicorp/http-echo`<br>`sha256:fcb75f691c8b0414d670ae570240cbf95502cc18a9ba57e982ecac589760a186` | 1 | 128 | 5678 | `-text={id}` | ✅ supported | HTTP 200 through the public URL, both machines answering |
| echo-server | `jmalloc/echo-server`<br>`sha256:86f2c45aa7e7ebe1be30b21f8cfff25a7ed6e3b059751822d4b35bf244a688d5` | 1 | 128 | 8080 | – | ✅ supported | HTTP 200 through the public URL, both machines answering |
| podinfo | `stefanprodan/podinfo`<br>`sha256:fa2f27c1a8561097e5144bfc0853eba6054d743b68b03de4cad4675616ddefb0` | 1 | 128 | 9898 | – | ✅ supported | HTTP 200 through the public URL, both machines answering |
| hello | `nginxdemos/hello`<br>`sha256:b28d1e16c676896aaa98ed9c36d406cc4f4289558487f15e37e04354720f807a` | 1 | 128 | 80 | – | ✅ supported | HTTP 200 through the public URL, both machines answering |
| excalidraw | `excalidraw/excalidraw`<br>`sha256:f7ee194addd607bf831d2af0f0a34463dd4225e426cf35199ef0b12a803398e9` | 1 | 128 | 80 | – | ✅ supported | HTTP 200 through the public URL, both machines answering; its JS bundles loaded |
| it-tools | `corentinth/it-tools`<br>`sha256:8b8128748339583ca951af03dfe02a9a4d7363f61a216226fc28030731a5a61f` | 1 | 128 | 80 | – | ✅ supported | HTTP 200 through the public URL, both machines answering; its JS bundles loaded |
| thelounge | `thelounge/thelounge`<br>`sha256:345af9c9d48d789f71a09d172fbc1bd3f250d47059c5a317497b67bd315989b7` | 1 | 256 | 9000 | – | ✅ supported | HTTP 200 through the public URL, both machines answering; its JS bundles loaded |

## What made the difference

- Guest kernel (commits bc61901, 65bb380, 18008d6): IPv6 (sws, n8n, adminer, nginx), a wall clock from
  Firecracker's RTC (loki, cockroach), cgroup v2 (redpanda, mariadb), inotify (dotnet, jaeger), a dummy
  eth0 with a private address (alertmanager), xattrs (vault).
- VOLUME directories created by the launcher (c11e23c): uptime-kuma.
- 2 vCPUs / 1 GiB instead of the default 1 / 128 MiB (24d85f1): mongo, kafka, rabbitmq, grafana, gitea,
  keycloak, clickhouse, nextcloud (memory, or startup within the readiness window).
- Env, per the images' docs: postgres, mysql, mariadb (root password), couchdb (admin user),
  opensearch (security plugin off, smaller heap, single-node), ghost (development mode: SQLite).
