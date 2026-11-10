# Soloist на Мини 2

Это экспериментальная интеграция, а не новая сборка Soloist из исходников.
Официальный ARM32 бинарник требует glibc 2.38; штатная система Мини 2 старее.
Также нужен рабочий PulseAudio/PipeWire или отдельно проверенный слой совместимости
PulseAudio → ALSA. `prepare.py` **не собирает** этот слой: параметр `--compat-dir`
должен указывать на уже подготовленные `ld.so` и `lib/` для вашей станции.
На Мини 2 проверены вход через Spotify Connect и воспроизведение с отдельной
glibc и apulse. Используется `plug:default`: штатный `default` не принимает
FLOAT_LE напрямую. Громкостью управляет Spotiya через ALSA Master;
`SPOTIYA_SOLOIST_HARDWARE_VOLUME=1` отключает повторное приглушение в нашем apulse.
Без этого при 30% два регулятора вместе ослабляют сигнал примерно на 67 дБ.

На ядре ARM64 Мини 2 launcher устанавливает `/proc/sys/abi/cp15_barrier=2`:
аппаратное исполнение CP15 barriers. Эмуляция (`1`) сбрасывает exclusive monitor
в атомарных операциях официального ARM32 бинарника и вызывает зависание.
Настройка общая для ядра и действует до перезагрузки; код Soloist не патчится.

## Слой PulseAudio → ALSA

Патч `apulse-spotiya.patch` рассчитан на apulse
`5d654cecd18474b4e0d885e774bc41fcbbc9818b` из
[upstream](https://github.com/i-rinat/apulse).
Спектр анализируется в `apulse-spectrum.c`: четыре полосы 100/400/1600/6400 Гц,
20 обновлений в секунду. В локальный сокет Spotiya передаются только четыре байта
уровней; PCM не сохраняется. Анализ выключается через обычную настройку спектра
(клавиша `e` в TUI, настройка перечитывается не реже раза в секунду во время
воспроизведения). Поддерживаются FLOAT32LE/S16LE, моно и стерео; частота берётся
из аудиопотока. Отсутствие или переполнение сокета не блокирует аудиовывод.

Приглушение во время диалога с Алисой — в `apulse-duck.c`: пока существует файл
`/run/spotiya-duck` не старше 65 секунд, поток убирается до 20% (спад 80 мс, возврат 300 мс).
Файл создаёт и удаляет Spotiya; устаревший файл игнорируется, чтобы аварийно завершившийся
приёмник не оставил музыку приглушённой. Тест рампы на ПК:

```sh
cc -std=gnu99 -O2 -Wall -Wextra -Werror -Isoloist soloist/test-apulse-duck.c soloist/apulse-duck.c -lm -o /tmp/test-apulse-duck
/tmp/test-apulse-duck
```

Кроме единого регулятора громкости, патч удерживает контекст и поток до завершения
отложенных операций и корректно завершает отменённые операции.

Сборка с уже подготовленными ARM32 sysroot (glibc/GLib и ALSA):

```sh
bash soloist/build-apulse.sh /path/to/apulse /path/to/cross-sdk /path/to/glibc-sysroot /path/to/alsa-sysroot /tmp/apulse-output
```

Скрипт применяет патч к временной копии исходников и собирает библиотеку и тесты.
Поместите `libpulse.so.0` в `lib/` вашего compatibility runtime. Тест не открывает
аудиоустройство; на станции его можно выполнить через loader, указав каталог
проверяемой библиотеки первым в `--library-path`:

```sh
/data/opt/solo/ld.so --library-path /tmp/apulse-test:/data/opt/solo/lib:/usr/lib:/lib /tmp/apulse-test/test-apulse-lifetime
```

На ПК тест спектра можно собрать так:

```sh
cc -std=gnu99 -O2 -Wall -Wextra -Werror soloist/test-apulse-spectrum.c -lm -o /tmp/test-apulse-spectrum
/tmp/test-apulse-spectrum
```

Тест проверяет полосы на 44,1/48 кГц, противофазное стерео, тишину, отсутствие
изменений PCM, выключение/включение и неблокирующую доставку. Он также пройден
на Мини 2. Столбики на дисплее и выключение/включение клавишей `e` в TUI
подтверждены пользователем при воспроизведении через Soloist.

Проверка на Мини 2: 400 сценариев времени жизни объектов прошли; исходная
библиотека на том же тесте падает с ошибкой GLib. Пользователь проверил звук,
паузу и повторное подключение Spotify Connect после установки исправления.
Это не полная реализация PulseAudio; переходы между всеми дополнениями ещё
не проверены.

Ограничение текущего стенда: Soloist разрешает лимит кэша от 100 МБ
(`--cache-size 0` означает отсутствие лимита, а не отключение). В интеграции
выбран 100 МБ, но весь раздел данных Мини 2 — около 87,5 МБ. Длительное
воспроизведение пока не проверено: нужно решить размещение/ограничение кэша,
прежде чем оставлять этот стенд работать постоянно. После короткого прогона
кэш занял 13,8 МБ, свободно 14,6 МБ; RSS Soloist около 41 МБ.

## Подготовка официального runtime

## Установка из дополнений

После установки CLI или TUI запрашивает API-ключ, если его ещё нет. Ввод скрыт;
обновление сохраняет существующий ключ. Enter на установленном Soloist в TUI
позволяет заменить ключ. Для повторного ввода из терминала: `/data/opt/solo/set-key.sh`.


Из релиза с пакетом совместимости для вашей версии Spotiya:

```sh
spotiya addon install soloist
spotiya backend soloist
spotiya restart
```

В TUI: `a` → Spotify Soloist → Enter; `u` обновляет, `x` дважды удаляет.
Удаление требует остановленного приёмника; ключ и данные входа остаются в
`receiver-state/soloist`. Полное удаление Spotiya удаляет и управляемый runtime.
Старые ручные установки без метки владельца автоматически при удалении Spotiya
не удаляются.

GitHub отдаёт **только** `spotiya-soloist-compat-armv7.tar.gz` и
`SOLOIST_SHA256SUMS`. Официальный клиент скачивается отдельно, непосредственно
из `https://soloist-builds.spotifycdn.com/soloist_release_arm32.tar.gz`, с проверкой
TLS и без перенаправлений. SHA256 нашего пакета проверяется по релизу, каждого
исполняемого файла — по манифесту. Для официального клиента сохраняются исходный
и итоговый хэши; это учёт скачанного по HTTPS файла, а не подпись Spotify.
Проверяются ARM ELF, границы PT_INTERP и запуск `--version` через новый loader.
Ключ не нужен для скачивания/проверки и не передаётся GitHub.

Обновление не перезапускает музыку автоматически: после него выполните
`spotiya retry`. Во время обновления нужны место для обеих копий runtime и
временное место под скачиваемый архив. При нехватке места старые файлы не заменяются.
При двойной ошибке переименования старая копия остаётся вне временного каталога;
путь к ней выводится в ошибке.

Для локальной сборки и проверки до публикации GitHub-пакета:

```sh
scripts/bootstrap-arm.sh
python3 soloist/build.py
adb push dist/spotiya-soloist-compat-armv7.tar.gz dist/SOLOIST_SHA256SUMS /tmp/
# На станции; официальный клиент всё равно скачивается с Spotify:
spotiya addon file soloist /tmp/spotiya-soloist-compat-armv7.tar.gz /tmp/SOLOIST_SHA256SUMS
```

`build.py` загружает зависимости по закреплённым хэшам из `sources.json`,
собирает apulse и пакует glibc/libatomic/скрипты/лицензии. Workflow публикует этот
пакет для тега вместе с остальными дополнениями. `package.py` использует явный
список файлов и отказывается принимать каталог с бинарником `soloist`.

На ПК (прежняя ручная подготовка):

```sh
python3 soloist/prepare.py --compat-dir /path/to/tested-arm32-runtime --output /tmp/soloist-runtime
adb -s STATION:5555 push /tmp/soloist-runtime /data/opt/solo
```

Каталог назначения должен быть новым; перед обновлением остановите Spotiya и
сохраните прежний runtime. Не копируйте старый или чужой API key.
`prepare.py` скачивает текущий официальный архив по HTTPS, сохраняет его хэш
и лицензии и меняет только путь ELF-интерпретатора на `/data/opt/solo/ld.so`.
Код Spotify и механизм авторизации не изменяются; UPX не применяется.
Архив можно передать явно через `--archive`. Хэш в `source.json` служит учёту
локального артефакта, а не является отдельной подписью Spotify.

На станции:

```sh
mkdir -p /data/opt/spotify-connect/receiver-state/soloist
chmod 700 /data/opt/spotify-connect/receiver-state/soloist
printf '%s\n' /data/opt/solo/run.sh > /data/opt/spotify-connect/receiver-state/soloist/launcher
/data/opt/solo/set-key.sh
spotiya backend soloist
spotiya restart
```

`set-key.sh` читает скрытый ввод с терминала. Если ключ уже лежит в приватном
файле, можно использовать `spotiya soloist key < FILE`. Старый ключ от
заблокированного аккаунта не переносится автоматически.

Состояние: `spotiya status`. Диагностика: `/tmp/spotiya-soloist.log` (0600).
API слушает только `127.0.0.1`, порт выбирает ОС. При сбое Soloist входы автоматически
не повторяются; после устранения причины выполните `spotiya retry`.
Сборки Soloist имеют срок действия: код выхода 10 означает необходимость скачать
свежую официальную сборку. Собранный каталог содержит проприетарный файл:
**не публикуйте его в релизах**, каждый пользователь скачивает Soloist у Spotify.

Источники: [установка](https://developer.spotify.com/documentation/soloist/tutorials/getting-started),
[CLI](https://developer.spotify.com/documentation/soloist/reference/command-line),
[WebSocket API](https://developer.spotify.com/documentation/soloist/reference/websocket-api).
