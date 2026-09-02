# 한국어 빠른 안내

> 영어 문서가 `codex-switch-global-pace`의 기준 문서입니다. 이 페이지는
> 설치와 일상적인 계정 전환을 위한 한국어 요약이며 별도의 동작 명세는
> 아닙니다.

`codex-switch-global-pace`는 여러 Codex 로그인 정보를 이 컴퓨터에 저장하고,
계정별 사용량과 Global Weekly Pace를 보여 주며, 다음에 시작하는 Codex가 쓸
활성 계정을 선택합니다. 이 프로젝트는 OpenAI의 공식 제품이 아니며 OpenAI와
제휴하거나 보증받지 않았습니다. 본인이 소유하거나 사용 권한을 받은 계정에만
사용하세요.

Global Weekly Pace는 로컬 합산 화면입니다. 계정 사이에서 quota를 이전하거나
합치거나 제한을 우회하지 않습니다. 계정을 전환하면 로컬
`$CODEX_HOME/auth.json`만 바뀌며 이미 실행 중인 Codex 세션에는 적용되지
않습니다.

## 가장 많이 쓰는 방법: Codex Windows 앱 계정 전환

Codex Windows 앱은 창을 닫아도 트레이에서 계속 실행될 수 있습니다. 다음
순서를 지키는 것이 가장 확실합니다.

**계정 전환 키:** `↑`/`↓`(또는 `j`/`k`) → `Enter` → `u`

1. 진행 중인 작업을 마칩니다.
2. Codex 앱 창을 닫습니다.
3. Windows 작업 표시줄의 숨겨진 아이콘을 열고 Codex 데스크톱 앱의
   **ChatGPT** 트레이 아이콘을 우클릭한 뒤 **Quit** 또는 **Exit**를
   선택합니다. 앱 버전에 따라 아이콘 이름이 **Codex**로 보일 수도 있습니다.
4. 해당 트레이 아이콘이 사라진 것을 확인합니다.
5. PowerShell에서 대시보드를 실행합니다. Windows Terminal을 쓴다면 WSL
   셸이 아닌 **PowerShell 프로필**을 여세요. WSL은 기본적으로 Windows 앱의
   `%USERPROFILE%\.codex`와 다른 홈 디렉터리를 사용합니다.

   ```powershell
   codex-switch-global-pace
   ```

6. `↑`/`↓` 또는 `j`/`k`로 원하는 계정을 선택하고 `Enter`를 눌러 해당 계정
   메뉴를 연 다음 `u`(**Use**)를 누릅니다. `Enter`와 `u`는 둘 중 하나를
   누르는 단축키가 아니라 차례로 실행하는 동작입니다.
7. `Switched to <alias>` 메시지를 확인한 뒤 `q`를 눌러 대시보드를 닫습니다.
8. Codex Windows 앱을 다시 실행합니다.

### 명령줄로 바로 전환하기

Codex를 완전히 종료한 뒤 명령줄에서 직접 전환하려면 다음 명령을 사용합니다.

```powershell
# 저장된 계정과 최신 사용량 확인
codex-switch-global-pace list -f

# 지정한 계정으로 전환
codex-switch-global-pace use work

# 또는 사용량을 기준으로 가장 적합한 계정을 자동 선택
codex-switch-global-pace use
```

Codex CLI도 동일합니다. 실행 중인 `codex`, `codex resume`, `codex exec`
세션을 모두 종료한 다음 계정을 전환하고 새 세션을 시작하세요.

## 처음 설정하기

Codex는 [file credential store](https://learn.chatgpt.com/docs/auth)를
사용해야 합니다. `$CODEX_HOME/config.toml`
(Windows에서는 보통 `%USERPROFILE%\.codex\config.toml`)에 다음 설정을
넣습니다.

```toml
cli_auth_credentials_store = "file"
```

설치에는 GitHub CLI의 현재 지원 버전과 GitHub 인증이 필요합니다.

```powershell
gh --version
gh auth login
gh auth status
```

그다음 [검증 설치 절차](Getting-Started.md#install)를 사용하세요. 설치 파일과
Release 출처 검증이 하나라도 실패하면 설치가 중단되며, 검증되지 않은 설치로
자동 전환되지 않습니다.

## personal과 work 계정 등록

브라우저가 이전 계정에 로그인된 상태일 수 있으므로 각 단계에서 표시되는
이메일과 workspace를 확인하세요.

```powershell
codex-switch-global-pace login personal
# 브라우저에서 personal 계정 확인

codex-switch-global-pace login work
# 필요하면 브라우저 계정을 바꾼 뒤 work 계정 확인

codex-switch-global-pace list -f
```

새 alias에서 인증한 계정이 기존 프로필과 다른 실제 계정이면 저장되는 동시에
그 alias가 활성 계정이 됩니다. 이미 저장된 실제 계정과 일치하면 기존
프로필이 갱신·활성화되고 요청한 새 alias는 만들어지지 않습니다. identity가
완전한 기존 alias로 다시 로그인할 때는 같은 실제 계정만 재인증할 수 있으며,
다른 계정으로 덮어쓸 수 없습니다. 그 프로필이 원래 비활성 상태였다면 재인증 뒤
`use <alias>`를 실행해야 활성 계정으로 바뀝니다. 잘못된 브라우저 계정을
저장했다면 [복구 절차](Troubleshooting.md#correct-a-wrong-browser-account)를
따르세요. alias는 1~64바이트의 ASCII 영문자·숫자와 `_`, `-`, `.`만 사용할 수
있으며 `.`과 `..`은 사용할 수 없습니다.

새 OAuth 인증 정보는 비어 있지 않은 `account_id`와 이메일을 모두 포함해야만
저장됩니다. 예전 버전에서 만든 프로필에 둘 중 하나가 없다면
`login <alias>`가 기본값 **No**인 확인을 요청합니다. 승인하면 기존 인증 정보를
`deleted-profiles/`에 정확히 보관한 뒤, 이미 알고 있던 identity 항목과 새 인증
계정이 일치할 때만 같은 alias를 새 인증 정보로 교체합니다. JSON이나 다른
비대화형 실행에서는 `login <alias> --yes`를 명시해야 합니다.

브라우저가 없는 서버에서는 `login --device`를 사용할 수 있습니다. 조직에서
device-code 로그인을 제한한 경우에는 관리자 정책에 따라 사용할 수 없으며,
자세한 오류 대응은 [Troubleshooting](Troubleshooting.md)을 확인하세요.

## 인증 정보와 삭제

- live 인증은 `$CODEX_HOME/auth.json`에 저장됩니다.
- 저장 프로필과 앱 상태는 기본적으로 `~/.codex-switch`에 있습니다.
- `delete`는 프로필을 즉시 파기하지 않고 `deleted-profiles/`로 옮깁니다.
- uninstall은 재설치와 원래 `codex-switch` 호환성을 위해 프로필을 보존합니다.

서비스가 일회성 refresh token을 교체하면 반환된 rotation material은 프로필을
변경하기 전에 private `recovery/` 디렉터리에 먼저 안전하게 기록됩니다. 프로필이
내구성 있게 저장되기 전의 충돌과 실패는 그 material을 보존해 알립니다. 프로필
저장 뒤 live auth 반영만 실패하면 exact stage가 이미 정리되어 복구 경로가 없을
수도 있습니다. 어떤 실패도 이미 사용된 token을 임의로 재시도하지 않습니다.
복구 경로는 원래 stage의 파일 identity가 그 위치에 그대로
있다고 확인될 때만 알리며, 다른 파일로 바뀌었거나 확인할 수 없으면 경로를
복구 파일이라고 단정하지 않고 부분 완료 상태만 정확히 보고합니다.

따라서 앱만 제거하는 것과 모든 credential을 영구 삭제하는 것은 다릅니다.
삭제·백업·새 PC 이전 절차는 [Configuration](Configuration.md)과
[Updating](Updating.md#uninstall-the-application)을 확인하세요. `auth.json`, profile, token,
proxy credential, 이메일·account ID가 포함된 debug 출력은 공유하거나
저장소에 올리지 마세요.

## Next steps

- [시작하기](Getting-Started.md) — 요구사항, 검증 설치, 다계정 등록
- [기능 가이드](Feature-Guide.md) — 전환, quota, TUI, daemon
- [명령어 참고](Command-Reference.md) — 명령과 옵션
- [설정](Configuration.md) — 경로, 보안, proxy, 백업
- [문제 해결](Troubleshooting.md) — 로그인·전환·복구 오류
- [보안 정책](../../SECURITY.md) — 민감한 취약점의 비공개 신고
