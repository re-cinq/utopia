type SsoStatus = { enabled: boolean; password_login: boolean };

/** Which blocks the login page shows. When password login is off and SSO is not properly configured either, keep the form: never lock everyone out */
export function loginSections(status: SsoStatus | undefined) {
  const sso = !!status?.enabled;
  return { password: !sso || status?.password_login !== false, sso };
}

/** Which wording to look up when SSO fails. The password form is hidden, so do not tell people to 'log in with a password first' again */
export function ssoErrorKey(code: string, passwordShown: boolean) {
  return code === "unlinked" && !passwordShown ? "unlinked_sso_only" : code;
}
