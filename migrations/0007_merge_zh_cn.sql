-- zh-cn was a second Simplified Chinese file under the same name in the language list; it is merged into zh-hans.
UPDATE users SET language_code = 'zh-hans' WHERE language_code = 'zh-cn';
