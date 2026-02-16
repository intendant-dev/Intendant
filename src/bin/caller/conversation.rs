use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    pub fn new(system_prompt: String) -> Self {
        Self {
            messages: vec![Message {
                role: "system".to_string(),
                content: system_prompt,
            }],
        }
    }

    pub fn add_user(&mut self, content: String) {
        self.messages.push(Message {
            role: "user".to_string(),
            content,
        });
    }

    pub fn add_assistant(&mut self, content: String) {
        self.messages.push(Message {
            role: "assistant".to_string(),
            content,
        });
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn estimated_tokens(&self) -> usize {
        self.messages.iter().map(|m| m.content.len() / 4).sum()
    }

    pub fn drop_turns(&mut self, indices: &[usize]) {
        let len = self.messages.len();
        let protected_min = if len >= 2 { len - 2 } else { len };

        let mut to_remove: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&i| i > 0 && i < protected_min)
            .collect();

        to_remove.sort_unstable();
        to_remove.dedup();

        // Remove in reverse order to preserve indices
        for &i in to_remove.iter().rev() {
            self.messages.remove(i);
        }
    }

    pub fn summarize_turns(&mut self, indices: &[usize], summary: &str) {
        if indices.is_empty() {
            return;
        }

        let len = self.messages.len();
        let protected_min = if len >= 2 { len - 2 } else { len };

        let mut valid: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&i| i > 0 && i < protected_min)
            .collect();

        valid.sort_unstable();
        valid.dedup();

        if valid.is_empty() {
            return;
        }

        let insert_pos = valid[0];

        // Remove in reverse order
        for &i in valid.iter().rev() {
            self.messages.remove(i);
        }

        self.messages.insert(
            insert_pos,
            Message {
                role: "user".to_string(),
                content: format!("[Context Summary] {}", summary),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_conversation_has_system_prompt() {
        let conv = Conversation::new("You are a helpful assistant.".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "You are a helpful assistant.");
    }

    #[test]
    fn add_user_message() {
        let mut conv = Conversation::new("system".to_string());
        conv.add_user("hello".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "hello");
    }

    #[test]
    fn add_assistant_message() {
        let mut conv = Conversation::new("system".to_string());
        conv.add_assistant("response".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content, "response");
    }

    #[test]
    fn conversation_ordering() {
        let mut conv = Conversation::new("sys".to_string());
        conv.add_user("msg1".to_string());
        conv.add_assistant("resp1".to_string());
        conv.add_user("msg2".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[2].role, "assistant");
        assert_eq!(msgs[3].role, "user");
    }

    #[test]
    fn message_serialization() {
        let msg = Message {
            role: "user".to_string(),
            content: "test".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.role, "user");
        assert_eq!(deserialized.content, "test");
    }

    #[test]
    fn len_and_estimated_tokens() {
        let mut conv = Conversation::new("system prompt".to_string());
        assert_eq!(conv.len(), 1);
        conv.add_user("hello world".to_string());
        assert_eq!(conv.len(), 2);
        // estimated_tokens is len/4 per message
        let tokens = conv.estimated_tokens();
        assert!(tokens > 0);
    }

    #[test]
    fn drop_turns_protects_system_and_last_two() {
        let mut conv = Conversation::new("sys".to_string());
        conv.add_user("u1".to_string());     // 1
        conv.add_assistant("a1".to_string()); // 2
        conv.add_user("u2".to_string());     // 3
        conv.add_assistant("a2".to_string()); // 4
        conv.add_user("u3".to_string());     // 5
        conv.add_assistant("a3".to_string()); // 6

        // Try to drop system (0), middle messages (1,2), and last two (5,6)
        conv.drop_turns(&[0, 1, 2, 5, 6]);

        // System (0) protected, last two (5,6) protected
        // Only 1 and 2 should be removed
        assert_eq!(conv.len(), 5); // 7 - 2 = 5
        assert_eq!(conv.messages()[0].role, "system");
        assert_eq!(conv.messages()[0].content, "sys");
    }

    #[test]
    fn drop_turns_empty_indices() {
        let mut conv = Conversation::new("sys".to_string());
        conv.add_user("u1".to_string());
        conv.drop_turns(&[]);
        assert_eq!(conv.len(), 2);
    }

    #[test]
    fn drop_turns_duplicate_indices() {
        let mut conv = Conversation::new("sys".to_string());
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user("u2".to_string());
        conv.add_assistant("a2".to_string());

        conv.drop_turns(&[1, 1, 1]);
        assert_eq!(conv.len(), 4); // only one removal
    }

    #[test]
    fn summarize_turns_replaces_range() {
        let mut conv = Conversation::new("sys".to_string());
        conv.add_user("u1".to_string());     // 1
        conv.add_assistant("a1".to_string()); // 2
        conv.add_user("u2".to_string());     // 3
        conv.add_assistant("a2".to_string()); // 4
        conv.add_user("u3".to_string());     // 5
        conv.add_assistant("a3".to_string()); // 6

        conv.summarize_turns(&[1, 2, 3, 4], "Set up the environment");

        // 7 original - 4 removed + 1 summary = 4
        assert_eq!(conv.len(), 4);
        assert_eq!(conv.messages()[0].content, "sys");
        assert!(conv.messages()[1].content.contains("[Context Summary]"));
        assert!(conv.messages()[1].content.contains("Set up the environment"));
        assert_eq!(conv.messages()[2].content, "u3");
        assert_eq!(conv.messages()[3].content, "a3");
    }

    #[test]
    fn summarize_turns_empty() {
        let mut conv = Conversation::new("sys".to_string());
        conv.add_user("u1".to_string());
        conv.summarize_turns(&[], "summary");
        assert_eq!(conv.len(), 2);
    }

    #[test]
    fn summarize_turns_protects_system_and_last_two() {
        let mut conv = Conversation::new("sys".to_string());
        conv.add_user("u1".to_string());     // 1
        conv.add_assistant("a1".to_string()); // 2

        // Try to summarize all — system (0) and last two (1,2) are protected
        conv.summarize_turns(&[0, 1, 2], "summary");
        assert_eq!(conv.len(), 3); // unchanged
    }
}
